import type * as vscode from "vscode";

const registryCacheKey = "importLens.registryHints";
const okCacheTtlMs = 6 * 60 * 60 * 1000;
const notFoundCacheTtlMs = 6 * 60 * 60 * 1000;
const errorCacheTtlMs = 5 * 60 * 1000;
const registryTimeoutMs = 3000;
const retryDelayMs = 500;
const maxAttempts = 3;

export interface RegistryHint {
  latestVersion?: string;
  latestPublishedAt?: string;
  isLatest?: boolean;
  deprecated?: boolean;
  fetchedAt?: number;
}

type RegistryHintCacheStatus = "ok" | "not_found" | "error";

interface RegistryHintCacheEntry extends RegistryHint {
  status: RegistryHintCacheStatus;
  timestamp: number;
  retryAfter?: number;
}

type RegistryHintCache = Record<string, RegistryHintCacheEntry>;
type LegacyRegistryHintCacheEntry = Omit<RegistryHintCacheEntry, "status"> & {
  status?: RegistryHintCacheStatus;
};

import type { Logger } from "../logging/types.js";

export interface RegistryHintFetchOptions {
  fetchImpl?: typeof fetch;
  forceRefresh?: boolean;
  installedVersion?: string;
  logger?: Pick<Logger, "debug" | "warn">;
  now?: () => number;
  sleep?: (delayMs: number) => Promise<void>;
  timeoutMs?: number;
}

const memoryCaches = new WeakMap<object, Map<string, RegistryHintCacheEntry>>();
const inFlightRequests = new Map<string, Promise<RegistryHint | null>>();

const concurrencyLimit = 5;
let activeRequests = 0;
const requestQueue: (() => void)[] = [];

const acquire = async (): Promise<void> => {
  if (activeRequests < concurrencyLimit) {
    activeRequests++;
    return;
  }
  return new Promise<void>((resolve) => requestQueue.push(resolve));
};

const release = (): void => {
  if (requestQueue.length > 0) {
    const next = requestQueue.shift()!;
    next();
  } else {
    activeRequests--;
  }
};

const defaultSleep = (delayMs: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, delayMs));

const cacheKeyForPackage = (packageName: string, installedVersion?: string): string =>
  `${packageName}\n${installedVersion ?? ""}`;

const cacheTtlForStatus = (status: RegistryHintCacheStatus): number => {
  if (status === "not_found") {
    return notFoundCacheTtlMs;
  }

  if (status === "error") {
    return errorCacheTtlMs;
  }

  return okCacheTtlMs;
};

const memoryCacheFor = (context: vscode.ExtensionContext): Map<string, RegistryHintCacheEntry> => {
  const key = context.globalState as object;
  const existing = memoryCaches.get(key);

  if (existing) {
    return existing;
  }

  const persisted = context.globalState.get<Record<string, LegacyRegistryHintCacheEntry>>(registryCacheKey, {});
  const hydrated = new Map(
    Object.entries(persisted).map(([key, entry]) => [key, hydrateCacheEntry(key, entry)]),
  );
  memoryCaches.set(key, hydrated);
  return hydrated;
};

const installedVersionFromCacheKey = (key: string): string | undefined => {
  const separatorIndex = key.indexOf("\n");

  if (separatorIndex === -1) {
    return undefined;
  }

  const installedVersion = key.slice(separatorIndex + 1);
  return installedVersion.length > 0 ? installedVersion : undefined;
};

const inferIsLatest = (
  installedVersion: string | undefined,
  latestVersion: string | undefined,
): boolean | undefined =>
  installedVersion && latestVersion ? installedVersion === latestVersion : undefined;

const hydrateCacheEntry = (
  key: string,
  entry: LegacyRegistryHintCacheEntry,
): RegistryHintCacheEntry => ({
  ...entry,
  status: entry.status ?? "ok",
  isLatest: entry.isLatest ?? inferIsLatest(installedVersionFromCacheKey(key), entry.latestVersion),
});

const cachedEntry = (
  context: vscode.ExtensionContext,
  packageName: string,
  installedVersion: string | undefined,
  now: number,
): RegistryHintCacheEntry | null => {
  const entry = memoryCacheFor(context).get(cacheKeyForPackage(packageName, installedVersion));

  if (!entry) {
    return null;
  }

  if (entry.retryAfter && entry.retryAfter > now) {
    return entry;
  }

  if (now - entry.timestamp < cacheTtlForStatus(entry.status)) {
    return entry;
  }

  return null;
};

const hintFromEntry = (entry: RegistryHintCacheEntry | null): RegistryHint | null => {
  if (entry?.status !== "ok") {
    return null;
  }

  const hint: RegistryHint = {};

  if (entry.latestVersion) {
    hint.latestVersion = entry.latestVersion;
  }

  if (entry.latestPublishedAt) {
    hint.latestPublishedAt = entry.latestPublishedAt;
  }

  if (typeof entry.isLatest === "boolean") {
    hint.isLatest = entry.isLatest;
  }

  if (typeof entry.deprecated === "boolean") {
    hint.deprecated = entry.deprecated;
  }

  hint.fetchedAt = entry.timestamp;
  return hint;
};

const storeEntry = async (
  context: vscode.ExtensionContext,
  packageName: string,
  installedVersion: string | undefined,
  entry: RegistryHintCacheEntry,
): Promise<void> => {
  const key = cacheKeyForPackage(packageName, installedVersion);
  const memoryCache = memoryCacheFor(context);
  memoryCache.set(key, entry);
  const persisted = Object.fromEntries(memoryCache.entries()) satisfies RegistryHintCache;
  await context.globalState.update(registryCacheKey, persisted);
};

const retryAfterDelayMs = (response: Response, now: number): number | null => {
  const header = response.headers.get("retry-after");

  if (!header) {
    return null;
  }

  const seconds = Number(header);

  if (Number.isFinite(seconds) && seconds >= 0) {
    return seconds * 1000;
  }

  const dateMs = Date.parse(header);

  if (Number.isFinite(dateMs) && dateMs > now) {
    return dateMs - now;
  }

  return null;
};

const fetchWithTimeout = async (
  url: string,
  fetchImpl: typeof fetch,
  timeoutMs: number,
): Promise<Response> => {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);

  try {
    return await fetchImpl(url, {
      signal: controller.signal,
      headers: { accept: "application/json" },
    });
  } finally {
    clearTimeout(timer);
  }
};

const registryHintFromMetadata = (
  metadata: {
    "dist-tags"?: { latest?: string };
    time?: Record<string, string>;
    versions?: Record<string, { deprecated?: unknown }>;
  },
  installedVersion?: string,
): RegistryHint => {
  const latestVersion = metadata["dist-tags"]?.latest;
  const latestPublishedAt = latestVersion ? metadata.time?.[latestVersion] : undefined;
  const versionForDeprecation = installedVersion ?? latestVersion;
  const deprecated = versionForDeprecation
    ? Boolean(metadata.versions?.[versionForDeprecation]?.deprecated)
    : false;
  const isLatest = inferIsLatest(installedVersion, latestVersion);
  const hint: RegistryHint = { deprecated };

  if (latestVersion) {
    hint.latestVersion = latestVersion;
  }

  if (latestPublishedAt) {
    hint.latestPublishedAt = latestPublishedAt;
  }

  if (typeof isLatest === "boolean") {
    hint.isLatest = isLatest;
  }

  return hint;
};

const fetchRegistryHintUncached = async (
  context: vscode.ExtensionContext,
  packageName: string,
  options: RegistryHintFetchOptions,
): Promise<RegistryHint | null> => {
  const fetchImpl = options.fetchImpl ?? fetch;
  const now = options.now ?? Date.now;
  const sleep = options.sleep ?? defaultSleep;
  const timeoutMs = options.timeoutMs ?? registryTimeoutMs;
  const url = `https://registry.npmjs.org/${packageName}`;

  await acquire();
  try {
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      options.logger?.debug(`Fetching npm registry hint for ${packageName} (attempt ${attempt}/${maxAttempts}).`);

      try {
        const response = await fetchWithTimeout(url, fetchImpl, timeoutMs);

        if (response.ok) {
          const metadata = await response.json() as Parameters<typeof registryHintFromMetadata>[0];
          const hint = registryHintFromMetadata(metadata, options.installedVersion);
          const timestamp = now();
          await storeEntry(context, packageName, options.installedVersion, {
            status: "ok",
            timestamp,
            latestVersion: hint.latestVersion,
            latestPublishedAt: hint.latestPublishedAt,
            isLatest: hint.isLatest,
            deprecated: hint.deprecated,
          });
          return { ...hint, fetchedAt: timestamp };
        }

        if (response.status === 404) {
          await storeEntry(context, packageName, options.installedVersion, {
            status: "not_found",
            timestamp: now(),
          });
          return null;
        }

        const retryAfter = response.status === 429 ? retryAfterDelayMs(response, now()) : null;

        if (attempt < maxAttempts) {
          await sleep(retryAfter ?? retryDelayMs);
          continue;
        }
      } catch {
        if (attempt < maxAttempts) {
          await sleep(retryDelayMs);
          continue;
        }
      }

      break;
    }
  } finally {
    release();
  }

  options.logger?.warn(`npm registry hint fetch failed for ${packageName} after ${maxAttempts} attempts.`);
  await storeEntry(context, packageName, options.installedVersion, {
    status: "error",
    timestamp: now(),
  });
  return null;
};

export const getCachedRegistryHint = (
  context: vscode.ExtensionContext,
  packageName: string,
  installedVersion?: string,
  options: Pick<RegistryHintFetchOptions, "now"> = {},
): RegistryHint | null =>
  hintFromEntry(cachedEntry(context, packageName, installedVersion, (options.now ?? Date.now)()));

export const fetchRegistryHint = async (
  context: vscode.ExtensionContext,
  packageName: string,
  options: RegistryHintFetchOptions = {},
): Promise<RegistryHint | null> => {
  const now = options.now ?? Date.now;
  const cached = cachedEntry(context, packageName, options.installedVersion, now());

  if (cached && !options.forceRefresh) {
    return hintFromEntry(cached);
  }

  const key = cacheKeyForPackage(packageName, options.installedVersion);
  const inFlight = inFlightRequests.get(key);

  if (inFlight) {
    return inFlight;
  }

  const request = fetchRegistryHintUncached(context, packageName, options)
    .finally(() => inFlightRequests.delete(key));
  inFlightRequests.set(key, request);
  return request;
};

export const registryHintForPackage = async (
  context: vscode.ExtensionContext,
  packageName: string,
  options: RegistryHintFetchOptions = {},
): Promise<RegistryHint | null> => {
  const cached = options.forceRefresh
    ? null
    : getCachedRegistryHint(context, packageName, options.installedVersion, options);
  if (cached) {
    return cached;
  }
  return fetchRegistryHint(context, packageName, options);
};
