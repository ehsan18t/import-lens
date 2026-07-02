export const nodeModulesInvalidationBurstLimit = 20;
const defaultNodeModulesInvalidationDelayMs = 250;

export type NodeModulesInvalidationDecision =
  | { kind: "none" }
  | { kind: "packages"; packageJsonPaths: string[] }
  | { kind: "all"; count: number };

export interface NodeModulesInvalidationSink {
  invalidateAll(): void;
  nodeModulesChanged(packageJsonPaths: readonly string[]): void;
}

export interface NodeModulesInvalidationBuffer {
  dispose(): void;
  flush(): void;
  queue(packageJsonPath: string): void;
}

export interface NodeModulesInvalidationBufferOptions {
  clearTimeoutFn?: (handle: unknown) => void;
  delayMs?: number;
  logger?: Pick<{ info(message: string): void }, "info">;
  onInvalidated?: () => void;
  setTimeoutFn?: (callback: () => void, delayMs: number) => unknown;
}

export const nodeModulesInvalidationDecision = (
  packageJsonPaths: readonly string[],
  burstLimit: number = nodeModulesInvalidationBurstLimit,
): NodeModulesInvalidationDecision => {
  const uniquePaths = [...new Set(packageJsonPaths.filter((path: string) => path.length > 0))];

  if (uniquePaths.length === 0) {
    return { kind: "none" };
  }

  if (uniquePaths.length > burstLimit) {
    return {
      kind: "all",
      count: uniquePaths.length,
    };
  }

  return {
    kind: "packages",
    packageJsonPaths: uniquePaths,
  };
};

export const createNodeModulesInvalidationBuffer = (
  sink: NodeModulesInvalidationSink,
  options: NodeModulesInvalidationBufferOptions = {},
): NodeModulesInvalidationBuffer => {
  const pending = new Set<string>();
  const delayMs = options.delayMs ?? defaultNodeModulesInvalidationDelayMs;
  const setTimeoutFn = options.setTimeoutFn ?? setTimeout;
  const clearTimeoutFn = options.clearTimeoutFn ?? ((handle) => clearTimeout(handle as ReturnType<typeof setTimeout>));
  let timer: unknown;
  let disposed = false;

  const clearTimer = (): void => {
    if (timer === undefined) {
      return;
    }

    clearTimeoutFn(timer);
    timer = undefined;
  };

  const flush = (): void => {
    clearTimer();

    if (disposed) {
      pending.clear();
      return;
    }

    const decision = nodeModulesInvalidationDecision([...pending]);
    pending.clear();

    if (decision.kind === "none") {
      return;
    }

    if (decision.kind === "all") {
      sink.invalidateAll();
      options.logger?.info(
        `Queued ${decision.count} node_modules package.json invalidation(s); invalidating all ImportLens caches.`,
      );
      options.onInvalidated?.();
      return;
    }

    sink.nodeModulesChanged(decision.packageJsonPaths);
    options.logger?.info(
      `Queued ${decision.packageJsonPaths.length} node_modules package.json invalidation(s).`,
    );
    options.onInvalidated?.();
  };

  return {
    dispose: () => {
      disposed = true;
      clearTimer();
      pending.clear();
    },
    flush,
    queue: (packageJsonPath) => {
      if (disposed) {
        return;
      }

      pending.add(packageJsonPath);
      clearTimer();
      timer = setTimeoutFn(flush, delayMs);
    },
  };
};
