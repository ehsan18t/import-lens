import assert from "node:assert/strict";
import test from "node:test";
import { flushNodeModulesInvalidations } from "../src/watcherActions.js";

test("flushNodeModulesInvalidations invalidates changed packages and refreshes visible analysis once", () => {
  const calls: string[] = [];

  flushNodeModulesInvalidations(
    ["react", "lodash-es"],
    {
      invalidateAll: () => calls.push("all"),
      invalidatePackage: (packageName) => calls.push(`package:${packageName}`),
    },
    () => calls.push("refresh"),
  );

  assert.deepEqual(calls, ["package:react", "package:lodash-es", "refresh"]);
});

test("flushNodeModulesInvalidations refreshes visible analysis after broad invalidation", () => {
  const calls: string[] = [];
  const packages = Array.from({ length: 21 }, (_, index) => `package-${index}`);

  flushNodeModulesInvalidations(
    packages,
    {
      invalidateAll: () => calls.push("all"),
      invalidatePackage: (packageName) => calls.push(`package:${packageName}`),
    },
    () => calls.push("refresh"),
  );

  assert.deepEqual(calls, ["all", "refresh"]);
});
