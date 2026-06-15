import type { Logger } from "./logging/types.js";

export interface NodeModulesInvalidationTarget {
  invalidateAll(): void;
  invalidatePackage(packageName: string): void;
}

export const flushNodeModulesInvalidations = (
  packages: Iterable<string>,
  target: NodeModulesInvalidationTarget,
  onInvalidated?: () => void,
  logger?: Pick<Logger, "info">,
): void => {
  const packageNames = [...packages];

  if (packageNames.length === 0) {
    return;
  }

  if (packageNames.length > 20) {
    logger?.info(`Invalidating entire ImportLens cache after ${packageNames.length} node_modules changes.`);
    target.invalidateAll();
  } else {
    for (const packageName of packageNames) {
      logger?.info(`Invalidating ImportLens cache for ${packageName}.`);
      target.invalidatePackage(packageName);
    }
  }

  onInvalidated?.();
};
