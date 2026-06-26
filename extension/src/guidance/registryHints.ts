import PQueue from "p-queue";
import type { RegistryHint } from "../ipc/protocol.js";
import type { Logger } from "../logging/types.js";

export interface RegistryHintStore {
  get<T>(key: string, defaultValue: T): T;
  update(key: string, value: unknown): Thenable<void> | Promise<void> | void;
}

export interface RegistryHintCacheOptions {
  readonly allowStale?: boolean;
  readonly now?: RegistryClock;
}

export interface FetchRegistryHintOptions {
  readonly installedVersion?: string;
  readonly forceRefresh?: boolean;
  readonly fetchImpl?: RegistryFetch;
  readonly sleep?: RegistrySleep;
  readonly now?: RegistryClock;
  readonly timeoutMs?: number;
  readonly logger?: Pick<Logger, "debug" | "warn">;
}

export type RegistryClock = number | (() => number);
export type RegistryFetch = (input: string, init?: RequestInit) => Promise<Response>;
export type RegistrySleep = (ms: number) => Promise<void>;

interface RegistryHintCacheEntry {
  readonly hint?: RegistryHint | null;
  readonly updatedAt: number;
  readonly retryAfter?: number;
  readonly error?: string;
  readonly notFound?: boolean;
}

type RegistryHintCache = Record<string, RegistryHintCacheEntry>;

interface PackageMetadata {
  readonly "dist-tags"?: Record<string, string>;
  readonly versions?: Record<string, { readonly deprecated?: unknown }>;
  readonly time?: Record<string, string>;
}

const registryCacheStorageKey = "importLens.registryHints";
const freshHintTtlMs = 6 * 60 * 60 * 1000;
const notFoundTtlMs = 6 * 60 * 60 * 1000;
const transientErrorRetryMs = 5 * 60 * 1000;
const defaultTimeoutMs = 3000;
const maxAttempts = 3;

export const registryQueueDefaults: Readonly<{
  concurrency: number;
  intervalCap: number;
  interval: number;
}> = Object.freeze({
  concurrency: 8,
  intervalCap: 20,
  interval: 1000,
});

let registryQueue = new PQueue({
  ...registryQueueDefaults,
  carryoverIntervalCount: true,
});
const inflightRegistryRequests = new Map<string, Promise<RegistryHint | null>>();
let sessionRegistryCaches = new WeakMap<object, RegistryHintCache>();

export const getCachedRegistryHint = (
  store: RegistryHintStore,
  packageName: string,
  installedVersion?: string,
  options: RegistryHintCacheOptions = {},
): RegistryHint | null => {
  const entry = readRegistryCache(store)[registryCacheKey(packageName, installedVersion)];

  if (!entry?.hint) {
    return null;
  }

  if (options.allowStale || isFreshEntry(entry, nowMs(options.now), freshHintTtlMs)) {
    return entry.hint;
  }

  return null;
};

export const fetchRegistryHint = (
  store: RegistryHintStore,
  packageName: string,
  options: FetchRegistryHintOptions = {},
): Promise<RegistryHint | null> => {
  const installedVersion = options.installedVersion;
  const targetKey = registryCacheKey(packageName, installedVersion);
  const current = readRegistryCache(store)[targetKey];
  const currentTime = nowMs(options.now);

  if (!options.forceRefresh) {
    if (current?.retryAfter && current.retryAfter > currentTime) {
      return Promise.resolve(current.hint ?? null);
    }

    if (current?.hint && isFreshEntry(current, currentTime, freshHintTtlMs)) {
      return Promise.resolve(current.hint);
    }

    if (current?.notFound && isFreshEntry(current, currentTime, notFoundTtlMs)) {
      return Promise.resolve(null);
    }
  }

  const inflight = inflightRegistryRequests.get(targetKey);

  if (inflight) {
    return inflight;
  }

  const task = registryQueue.add(() =>
    fetchAndCacheRegistryHint(store, packageName, installedVersion, options));
  const request = Promise.resolve(task).finally(() => {
    inflightRegistryRequests.delete(targetKey);
  });

  inflightRegistryRequests.set(targetKey, request);
  return request;
};

export const resetRegistryHintStateForTests = (): void => {
  registryQueue.clear();
  registryQueue = new PQueue({
    ...registryQueueDefaults,
    carryoverIntervalCount: true,
  });
  inflightRegistryRequests.clear();
  sessionRegistryCaches = new WeakMap<object, RegistryHintCache>();
};

const fetchAndCacheRegistryHint = async (
  store: RegistryHintStore,
  packageName: string,
  installedVersion: string | undefined,
  options: FetchRegistryHintOptions,
): Promise<RegistryHint | null> => {
  const fetchImpl = options.fetchImpl ?? fetch;
  const sleep = options.sleep ?? defaultSleep;
  const targetKey = registryCacheKey(packageName, installedVersion);
  let lastError: string | undefined;

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      const response = await fetchWithTimeout(fetchImpl, registryUrl(packageName), options.timeoutMs ?? defaultTimeoutMs);

      if (response.status === 404) {
        await writeRegistryCacheEntry(store, targetKey, {
          hint: null,
          updatedAt: nowMs(options.now),
          notFound: true,
        });
        return null;
      }

      if (response.status === 429) {
        const retryAfterMs = retryAfterDelayMs(response.headers.get("Retry-After")) ?? transientBackoffMs(attempt);
        await writeRegistryCacheEntry(store, targetKey, {
          ...readRegistryCache(store)[targetKey],
          updatedAt: nowMs(options.now),
          retryAfter: nowMs(options.now) + retryAfterMs,
          error: "npm registry rate limit",
        });

        if (attempt < maxAttempts) {
          await sleep(retryAfterMs);
          continue;
        }

        return readRegistryCache(store)[targetKey]?.hint ?? null;
      }

      if (!response.ok) {
        lastError = `npm registry responded with ${response.status}`;

        if (attempt < maxAttempts && isTransientStatus(response.status)) {
          await sleep(transientBackoffMs(attempt));
          continue;
        }

        break;
      }

      const metadata = await response.json() as PackageMetadata;
      const hint = registryHintFromMetadata(metadata, installedVersion, nowMs(options.now));
      await writeRegistryCacheEntry(store, targetKey, {
        hint,
        updatedAt: hint.fetchedAt ?? nowMs(options.now),
      });
      return hint;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);

      if (attempt < maxAttempts) {
        await sleep(transientBackoffMs(attempt));
        continue;
      }
    }
  }

  options.logger?.debug(`Registry hint fetch failed for ${packageName}: ${lastError ?? "unknown error"}`);
  await writeRegistryCacheEntry(store, targetKey, {
    ...readRegistryCache(store)[targetKey],
    updatedAt: nowMs(options.now),
    retryAfter: nowMs(options.now) + transientErrorRetryMs,
    error: lastError,
  });
  return readRegistryCache(store)[targetKey]?.hint ?? null;
};

const fetchWithTimeout = async (
  fetchImpl: RegistryFetch,
  url: string,
  timeoutMs: number,
): Promise<Response> => {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);

  try {
    return await fetchImpl(url, {
      headers: {
        accept: "application/vnd.npm.install-v1+json, application/json",
      },
      signal: controller.signal,
    });
  } finally {
    clearTimeout(timer);
  }
};

const registryHintFromMetadata = (
  metadata: PackageMetadata,
  installedVersion: string | undefined,
  fetchedAt: number,
): RegistryHint => {
  const latestVersion = metadata["dist-tags"]?.latest;
  const versionMetadata = installedVersion ? metadata.versions?.[installedVersion] : undefined;
  const deprecated = versionMetadata?.deprecated !== undefined;

  return {
    latestVersion,
    latestPublishedAt: latestVersion ? metadata.time?.[latestVersion] : undefined,
    isLatest: installedVersion && latestVersion ? installedVersion === latestVersion : undefined,
    deprecated,
    fetchedAt,
  };
};

const readRegistryCache = (store: RegistryHintStore): RegistryHintCache =>
  sessionRegistryCaches.get(store) ?? loadRegistryCache(store);

const writeRegistryCacheEntry = async (
  store: RegistryHintStore,
  targetKey: string,
  entry: RegistryHintCacheEntry,
): Promise<void> => {
  const cache = {
    ...readRegistryCache(store),
    [targetKey]: entry,
  };

  sessionRegistryCaches.set(store, cache);
  await Promise.resolve(store.update(registryCacheStorageKey, cache));
};

const loadRegistryCache = (store: RegistryHintStore): RegistryHintCache => {
  const cache = store.get<RegistryHintCache>(registryCacheStorageKey, {});
  sessionRegistryCaches.set(store, cache);
  return cache;
};

const registryCacheKey = (packageName: string, installedVersion?: string): string =>
  `${packageName}\n${installedVersion ?? ""}`;

const registryUrl = (packageName: string): string =>
  `https://registry.npmjs.org/${encodeURIComponent(packageName).replace(/^%40/, "@")}`;

const isFreshEntry = (
  entry: RegistryHintCacheEntry,
  currentTime: number,
  ttlMs: number,
): boolean =>
  currentTime - entry.updatedAt <= ttlMs;

const nowMs = (clock?: RegistryClock): number => {
  if (typeof clock === "function") {
    return clock();
  }

  return clock ?? Date.now();
};

const retryAfterDelayMs = (header: string | null): number | null => {
  if (!header) {
    return null;
  }

  const seconds = Number.parseFloat(header);

  if (Number.isFinite(seconds)) {
    return Math.max(0, Math.round(seconds * 1000));
  }

  const dateMs = Date.parse(header);

  if (Number.isNaN(dateMs)) {
    return null;
  }

  return Math.max(0, dateMs - Date.now());
};

const isTransientStatus = (status: number): boolean =>
  status === 408 || status === 425 || status === 429 || status >= 500;

const transientBackoffMs = (attempt: number): number =>
  (100 * attempt) + Math.floor(Math.random() * 100);

const defaultSleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));
