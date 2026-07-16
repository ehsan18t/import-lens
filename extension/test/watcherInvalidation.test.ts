import assert from "node:assert/strict";
import test from "node:test";
import {
  createNodeModulesInvalidationBuffer,
  isWorkspaceConfigPath,
  nodeModulesInvalidationBurstLimit,
  nodeModulesInvalidationDecision,
} from "../src/watcherInvalidation.js";

test("nodeModulesInvalidationDecision keeps small bursts package-scoped", () => {
  const decision = nodeModulesInvalidationDecision([
    "C:/workspace/node_modules/react/package.json",
    "C:/workspace/node_modules/vue/package.json",
    "C:/workspace/node_modules/react/package.json",
  ]);

  assert.deepEqual(decision, {
    kind: "changed",
    packageJsonPaths: [
      "C:/workspace/node_modules/react/package.json",
      "C:/workspace/node_modules/vue/package.json",
    ],
    tsconfigPaths: [],
  });
});

test("nodeModulesInvalidationDecision bulk-invalidates bursts over the limit", () => {
  const packageJsonPaths = Array.from(
    { length: nodeModulesInvalidationBurstLimit + 1 },
    (_, index) => `C:/workspace/node_modules/pkg-${index}/package.json`,
  );

  assert.deepEqual(nodeModulesInvalidationDecision(packageJsonPaths), {
    kind: "all",
    count: nodeModulesInvalidationBurstLimit + 1,
  });
});

test("nodeModulesInvalidationDecision ignores empty bursts", () => {
  assert.deepEqual(nodeModulesInvalidationDecision([]), { kind: "none" });
});

// The workspace's alias table is the ONLY thing that tells a path alias apart from a package that
// is not installed, and the daemon memoizes it. Until it was watched, the repair the SRS prescribes
// for an unrecognized alias -- add the `paths` entry -- did nothing for the daemon's whole life.
// The config half must reach the daemon, and it must NOT be laundered into the package half, where
// it would be mapped to a package name, fail, and (if it were the whole batch) trigger a full cache
// clear -- throwing away every measured package for a change that cannot alter what one weighs.
test("nodeModulesInvalidationDecision routes workspace configs to the tsconfig half", () => {
  const decision = nodeModulesInvalidationDecision([
    "C:/workspace/node_modules/react/package.json",
    "C:/workspace/tsconfig.json",
    "C:/workspace/tsconfig.app.json",
    "C:/workspace/jsconfig.json",
  ]);

  assert.deepEqual(decision, {
    kind: "changed",
    packageJsonPaths: ["C:/workspace/node_modules/react/package.json"],
    tsconfigPaths: [
      "C:/workspace/tsconfig.json",
      "C:/workspace/tsconfig.app.json",
      "C:/workspace/jsconfig.json",
    ],
  });
});

// A config edit on its own must still reach the daemon: it is the single deliberate keystroke the
// whole fix exists for.
test("nodeModulesInvalidationDecision reports a lone workspace config change", () => {
  assert.deepEqual(nodeModulesInvalidationDecision(["C:/workspace/tsconfig.json"]), {
    kind: "changed",
    packageJsonPaths: [],
    tsconfigPaths: ["C:/workspace/tsconfig.json"],
  });
});

// A dependency's own build config is not the workspace's alias table -- the daemon never reads one.
// An install rewrites thousands of them, and routing those into either half would either spam the
// daemon or (over the burst limit) nuke every cached measurement.
test("a tsconfig inside node_modules is not the workspace's alias table", () => {
  assert.equal(isWorkspaceConfigPath("C:/workspace/tsconfig.json"), true);
  assert.equal(isWorkspaceConfigPath("C:/workspace/node_modules/lib/tsconfig.json"), false);
  assert.deepEqual(
    nodeModulesInvalidationDecision(["C:/workspace/node_modules/lib/tsconfig.json"]),
    {
      kind: "none",
    },
  );
});

test("createNodeModulesInvalidationBuffer forwards both halves to the sink", () => {
  const calls: string[] = [];
  let scheduled: (() => void) | undefined;
  const buffer = createNodeModulesInvalidationBuffer(
    {
      invalidateAll: () => calls.push("all"),
      nodeModulesChanged: (packageJsonPaths, tsconfigPaths) =>
        calls.push(`packages:${packageJsonPaths.length} configs:${tsconfigPaths?.length ?? 0}`),
    },
    {
      setTimeoutFn: (callback) => {
        scheduled = callback;
        return "watcher-timer";
      },
    },
  );

  buffer.queue("C:/workspace/node_modules/react/package.json");
  buffer.queue("C:/workspace/tsconfig.json");
  scheduled?.();

  assert.deepEqual(calls, ["packages:1 configs:1"]);
});

test("createNodeModulesInvalidationBuffer clears pending timer on dispose", () => {
  const calls: string[] = [];
  let scheduled: (() => void) | undefined;
  let clearedHandle: string | undefined;
  const buffer = createNodeModulesInvalidationBuffer(
    {
      invalidateAll: () => calls.push("all"),
      nodeModulesChanged: (paths) => calls.push(`packages:${paths.length}`),
    },
    {
      clearTimeoutFn: (handle) => {
        clearedHandle = typeof handle === "string" ? handle : undefined;
      },
      setTimeoutFn: (callback) => {
        scheduled = callback;
        return "watcher-timer";
      },
    },
  );

  buffer.queue("C:/workspace/node_modules/react/package.json");
  buffer.dispose();
  scheduled?.();

  assert.equal(clearedHandle, "watcher-timer");
  assert.deepEqual(calls, []);
});
