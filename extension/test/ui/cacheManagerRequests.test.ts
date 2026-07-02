import assert from "node:assert/strict";
import test from "node:test";
import {
  cacheCleanupRequest,
  cacheListRequest,
  cacheRemoveAllRequest,
  cacheRemoveCurrentProjectRequest,
  cacheRemoveSelectedRequest,
  cacheStatusRequest,
} from "../../src/ui/cacheManagerRequests.js";

test("cache manager request builders use protocol version 7", () => {
  assert.deepEqual(cacheStatusRequest(1, "C:/workspace"), {
    type: "cache_status",
    version: 7,
    request_id: 1,
    workspace_root: "C:/workspace",
  });
  assert.deepEqual(cacheCleanupRequest(2), {
    type: "cache_cleanup",
    version: 7,
    request_id: 2,
  });
  assert.deepEqual(cacheListRequest(3), {
    type: "cache_list",
    version: 7,
    request_id: 3,
  });
  assert.deepEqual(cacheRemoveCurrentProjectRequest(4, "C:/workspace"), {
    type: "cache_remove",
    version: 7,
    request_id: 4,
    scope: "current_project",
    workspace_root: "C:/workspace",
  });
  assert.deepEqual(cacheRemoveSelectedRequest(5, ["v1-a", "v1-b"]), {
    type: "cache_remove",
    version: 7,
    request_id: 5,
    scope: "selected",
    shard_ids: ["v1-a", "v1-b"],
  });
  assert.deepEqual(cacheRemoveAllRequest(6), {
    type: "cache_remove",
    version: 7,
    request_id: 6,
    scope: "all",
  });
});
