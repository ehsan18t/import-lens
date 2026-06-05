import type * as vscode from "vscode";

const registryCacheKey = "importLens.registryHints";
const registryCacheTtlMs = 24 * 60 * 60 * 1000;
const registryTimeoutMs = 1500;

export interface RegistryHint {
  latestVersion?: string;
  deprecated?: boolean;
}

interface RegistryHintCacheEntry extends RegistryHint {
  timestamp: number;
}

type RegistryHintCache = Record<string, RegistryHintCacheEntry>;

export const registryHintForPackage = async (
  context: vscode.ExtensionContext,
  packageName: string,
): Promise<RegistryHint | null> => {
  const cache = context.globalState.get<RegistryHintCache>(registryCacheKey, {});
  const cached = cache[packageName];
  const now = Date.now();

  if (cached && now - cached.timestamp < registryCacheTtlMs) {
    return { latestVersion: cached.latestVersion, deprecated: cached.deprecated };
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), registryTimeoutMs);

  try {
    const response = await fetch(`https://registry.npmjs.org/${packageName}`, {
      signal: controller.signal,
      headers: { accept: "application/vnd.npm.install-v1+json" },
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
    return { latestVersion, deprecated };
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
};
