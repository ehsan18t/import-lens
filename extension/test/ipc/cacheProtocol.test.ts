import assert from "node:assert/strict";
import test from "node:test";
import {
  type CacheCleanupRequest,
  type CacheListRequest,
  type CacheRemoveRequest,
  type CacheStatusRequest,
  type ClientMessage,
  type HelloMessage,
  protocolVersion,
} from "../../src/ipc/protocol.js";

test("cache management protocol uses protocol version 7", () => {
  assert.equal(protocolVersion, 7);
});

test("hello message carries cache policy fields", () => {
  const hello: HelloMessage = {
    type: "hello",
    version: protocolVersion,
    workspace_root: "C:/workspace",
    storage_path: "C:/Code/User/workspaceStorage/importlens/daemon-cache",
    enable_disk_cache: true,
    cache_max_size_mb: 512,
    cache_max_age_days: 30,
    log_level: "info",
  };

  assert.equal(hello.cache_max_size_mb, 512);
  assert.equal(hello.cache_max_age_days, 30);
});

test("cache management requests are client messages", () => {
  const status: CacheStatusRequest = {
    type: "cache_status",
    version: protocolVersion,
    request_id: 1,
    workspace_root: "C:/workspace",
  };
  const cleanup: CacheCleanupRequest = {
    type: "cache_cleanup",
    version: protocolVersion,
    request_id: 2,
  };
  const list: CacheListRequest = {
    type: "cache_list",
    version: protocolVersion,
    request_id: 3,
  };
  const remove: CacheRemoveRequest = {
    type: "cache_remove",
    version: protocolVersion,
    request_id: 4,
    scope: "selected",
    shard_ids: ["v1-demo"],
  };
  const messages = [status, cleanup, list, remove] satisfies ClientMessage[];

  assert.deepEqual(
    messages.map((message) => message.type),
    ["cache_status", "cache_cleanup", "cache_list", "cache_remove"],
  );
});
