import assert from "node:assert/strict";
import test from "node:test";
import { resolveDaemonStartRoot } from "../../src/daemon/startRoot.js";

test("resolveDaemonStartRoot prefers an explicit analysis root", () => {
  assert.equal(resolveDaemonStartRoot("C:/app", "C:/workspace", "C:/previous"), "C:/app");
});

test("resolveDaemonStartRoot uses workspace root when no analysis root is provided", () => {
  assert.equal(resolveDaemonStartRoot(undefined, "C:/workspace", "C:/previous"), "C:/workspace");
});

test("resolveDaemonStartRoot falls back to the last successful analysis root for scheduled restarts", () => {
  assert.equal(
    resolveDaemonStartRoot(undefined, undefined, "C:/single-file-app"),
    "C:/single-file-app",
  );
});
