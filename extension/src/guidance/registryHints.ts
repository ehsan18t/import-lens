import type * as vscode from "vscode";

const registryCacheKey = "importLens.registryHints";
const registryCacheTtlMs = 24 * 60 * 60 * 1000;
const registryTimeoutMs = 800;

interface RegistryHintCacheEntry {
  timestamp: number;
  latestVersion?: string;
  deprecated?: boolean;
}

type RegistryHintCache = Record<string, RegistryHintCacheEntry>;

export const registryHintForPackage = async (
  context: vscode.ExtensionContext,
  packageName: string,
): Promise<string | null> => {
  const cache = context.globalState.get<RegistryHintCache>(registryCacheKey, {});
  const cached = cache[packageName];
  const now = Date.now();

  if (cached && now - cached.timestamp < registryCacheTtlMs) {
    return formatRegistryHint(cached);
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), registryTimeoutMs);

  try {
    const response = await fetch(`https://registry.npmjs.org/${encodeURIComponent(packageName)}`, {
      signal: controller.signal,
    });

    if (!response.ok) {
      return null;
    }

    const metadata = await response.json() as {
      "dist-tags"?: { latest?: string };
      versions?: Record<string, { deprecated?: unknown }>;
    };
    const latestVersion = metadata["dist-tags"]?.latest;
    const deprecated = latestVersion ? Boolean(metadata.versions?.[latestVersion]?.deprecated) : false;
    const entry = { timestamp: now, latestVersion, deprecated };
    await context.globalState.update(registryCacheKey, { ...cache, [packageName]: entry });
    return formatRegistryHint(entry);
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
};

const formatRegistryHint = (entry: RegistryHintCacheEntry): string | null => {
  if (entry.deprecated) {
    return "deprecated";
  }

  return entry.latestVersion ? `latest ${entry.latestVersion}` : null;
};
