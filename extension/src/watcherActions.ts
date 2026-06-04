export interface NodeModulesInvalidationTarget {
  invalidateAll(): void;
  invalidatePackage(packageName: string): void;
}

export const flushNodeModulesInvalidations = (
  packages: Iterable<string>,
  target: NodeModulesInvalidationTarget,
  onInvalidated?: () => void,
): void => {
  const packageNames = [...packages];

  if (packageNames.length === 0) {
    return;
  }

  if (packageNames.length > 20) {
    target.invalidateAll();
  } else {
    for (const packageName of packageNames) {
      target.invalidatePackage(packageName);
    }
  }

  onInvalidated?.();
};
