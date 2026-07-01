import assert from "node:assert/strict";
import test from "node:test";
import {
  createNodeModulesInvalidationBuffer,
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
    kind: "packages",
    packageJsonPaths: [
      "C:/workspace/node_modules/react/package.json",
      "C:/workspace/node_modules/vue/package.json",
    ],
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
