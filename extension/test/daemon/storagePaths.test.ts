import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import { resolveDaemonStoragePaths } from "../../src/daemon/storagePaths.js";

test("resolveDaemonStoragePaths uses workspace storage for cache and global storage for lifecycle", () => {
  const paths = resolveDaemonStoragePaths({
    storageUri: {
      fsPath: path.join("C:", "Code", "User", "workspaceStorage", "abc", "importlens"),
    },
    globalStorageUri: { fsPath: path.join("C:", "Code", "User", "globalStorage", "importlens") },
  });

  assert.equal(
    paths.cacheBasePath,
    path.join("C:", "Code", "User", "workspaceStorage", "abc", "importlens", "daemon-cache"),
  );
  assert.equal(
    paths.lifecycleStoragePath,
    path.join("C:", "Code", "User", "globalStorage", "importlens"),
  );
});

test("resolveDaemonStoragePaths falls back to global workspace-cache when workspace storage is unavailable", () => {
  const paths = resolveDaemonStoragePaths({
    globalStorageUri: { fsPath: path.join("C:", "Code", "User", "globalStorage", "importlens") },
  });

  assert.equal(
    paths.cacheBasePath,
    path.join("C:", "Code", "User", "globalStorage", "importlens", "workspace-cache"),
  );
  assert.equal(
    paths.lifecycleStoragePath,
    path.join("C:", "Code", "User", "globalStorage", "importlens"),
  );
});
