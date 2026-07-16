export const nodeModulesInvalidationBurstLimit = 20;
const defaultNodeModulesInvalidationDelayMs = 250;

/**
 * `tsconfig.json`, `jsconfig.json`, and the `tsconfig.app.json` / `tsconfig.node.json` files the
 * Vue and Astro scaffolds put the real `paths` table in (the root config there is nothing but
 * `references`). The daemon discovers `tsconfig.json` / `jsconfig.json` by name and then follows
 * `extends` and `references` into the rest, so an edit to any of them can change the alias table
 * and all of them must be watched.
 */
const workspaceConfigFileName = /^(?:ts|js)config(?:\.[^./\\]+)?\.json$/u;

const basenameOf = (candidate: string): string => candidate.split(/[/\\]/u).pop() ?? "";

const hasNodeModulesSegment = (candidate: string): boolean =>
  candidate.split(/[/\\]/u).includes("node_modules");

/**
 * Whether the path is one of the workspace's alias-table configs — the file that tells a path alias
 * apart from a package that is not installed. A `tsconfig.json` shipped INSIDE a package is not
 * one: the daemon never reads it, and a `pnpm install` would otherwise queue thousands of them.
 */
export const isWorkspaceConfigPath = (candidate: string): boolean =>
  workspaceConfigFileName.test(basenameOf(candidate)) && !hasNodeModulesSegment(candidate);

export type NodeModulesInvalidationDecision =
  | { kind: "none" }
  | { kind: "changed"; packageJsonPaths: string[]; tsconfigPaths: string[] }
  | { kind: "all"; count: number };

export interface NodeModulesInvalidationSink {
  invalidateAll(): void;
  nodeModulesChanged(packageJsonPaths: readonly string[], tsconfigPaths?: readonly string[]): void;
}

export interface NodeModulesInvalidationBuffer {
  dispose(): void;
  flush(): void;
  queue(changedPath: string): void;
}

export interface NodeModulesInvalidationBufferOptions {
  clearTimeoutFn?: (handle: unknown) => void;
  delayMs?: number;
  logger?: Pick<{ info(message: string): void }, "info">;
  onInvalidated?: () => void;
  setTimeoutFn?: (callback: () => void, delayMs: number) => unknown;
}

/**
 * Split a debounced burst of watched paths into the two things the daemon memoizes and cannot see
 * change: installed packages, and the workspace's **alias table**.
 *
 * The burst limit is a property of the package half alone — it is there because a `pnpm install`
 * rewrites hundreds of manifests at once and a full clear is cheaper than hundreds of targeted
 * ones. A config edit is a single deliberate keystroke; there is no burst to collapse, and a full
 * clear would throw away every measured package for a change that cannot alter what a package
 * weighs.
 *
 * A config inside `node_modules` reaches neither list: it is a dependency's own build config, the
 * daemon never reads it, and an install would otherwise queue thousands of them.
 */
export const nodeModulesInvalidationDecision = (
  changedPaths: readonly string[],
  burstLimit: number = nodeModulesInvalidationBurstLimit,
): NodeModulesInvalidationDecision => {
  const uniquePaths = [...new Set(changedPaths.filter((path: string) => path.length > 0))];
  const tsconfigPaths = uniquePaths.filter(isWorkspaceConfigPath);
  const packageJsonPaths = uniquePaths.filter(
    (path: string) => !workspaceConfigFileName.test(basenameOf(path)),
  );

  if (packageJsonPaths.length > burstLimit) {
    return {
      kind: "all",
      count: packageJsonPaths.length,
    };
  }

  if (packageJsonPaths.length === 0 && tsconfigPaths.length === 0) {
    return { kind: "none" };
  }

  return {
    kind: "changed",
    packageJsonPaths,
    tsconfigPaths,
  };
};

export const createNodeModulesInvalidationBuffer = (
  sink: NodeModulesInvalidationSink,
  options: NodeModulesInvalidationBufferOptions = {},
): NodeModulesInvalidationBuffer => {
  const pending = new Set<string>();
  const delayMs = options.delayMs ?? defaultNodeModulesInvalidationDelayMs;
  const setTimeoutFn = options.setTimeoutFn ?? setTimeout;
  const clearTimeoutFn =
    options.clearTimeoutFn ?? ((handle) => clearTimeout(handle as ReturnType<typeof setTimeout>));
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
        `Queued ${decision.count} node_modules package.json invalidation(s); invalidating all Import Lens caches.`,
      );
      options.onInvalidated?.();
      return;
    }

    sink.nodeModulesChanged(decision.packageJsonPaths, decision.tsconfigPaths);
    options.logger?.info(
      `Queued ${decision.packageJsonPaths.length} node_modules package.json and ${decision.tsconfigPaths.length} workspace config invalidation(s).`,
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
    queue: (changedPath) => {
      if (disposed) {
        return;
      }

      pending.add(changedPath);
      clearTimer();
      timer = setTimeoutFn(flush, delayMs);
    },
  };
};
