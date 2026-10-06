import assert from "node:assert/strict";
import test from "node:test";
import { protocolVersion } from "../../src/ipc/protocol.js";
import {
  cacheListRequest,
  cacheRemoveAllRequest,
  cacheRemoveCurrentProjectRequest,
  cacheRemoveRegistryRequest,
  cacheRemoveSelectedRequest,
  cacheStatusRequest,
} from "../../src/ui/cacheManagerRequests.js";

test("cache manager request builders use the current protocol version and correct scopes", () => {
  assert.deepEqual(cacheStatusRequest(1, "C:/workspace"), {
    type: "cache_status",
    version: protocolVersion,
    request_id: 1,
    workspace_root: "C:/workspace",
  });
  assert.deepEqual(cacheListRequest(3), {
    type: "cache_list",
    version: protocolVersion,
    request_id: 3,
  });
  // Clear current project -> current_project scope.
  assert.deepEqual(cacheRemoveCurrentProjectRequest(4, "C:/workspace"), {
    type: "cache_remove",
    version: protocolVersion,
    request_id: 4,
    scope: "current_project",
    workspace_root: "C:/workspace",
  });
  // Clear all projects -> selected scope carrying every shard id (leaves the
  // shared registry + resolvers untouched; there is no dedicated all-shards scope).
  assert.deepEqual(cacheRemoveSelectedRequest(5, ["v1-a", "v1-b"]), {
    type: "cache_remove",
    version: protocolVersion,
    request_id: 5,
    scope: "selected",
    shard_ids: ["v1-a", "v1-b"],
  });
  // Clear everything -> all scope (shards + registry + resolvers + derived).
  assert.deepEqual(cacheRemoveAllRequest(6), {
    type: "cache_remove",
    version: protocolVersion,
    request_id: 6,
    scope: "all",
  });
  // Clear registry metadata -> registry scope.
  assert.deepEqual(cacheRemoveRegistryRequest(8), {
    type: "cache_remove",
    version: protocolVersion,
    request_id: 8,
    scope: "registry",
  });
});
