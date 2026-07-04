import {
  type CacheCleanupRequest,
  type CacheListRequest,
  type CacheRemoveRequest,
  type CacheStatusRequest,
  protocolVersion,
} from "../ipc/protocol.js";

export const cacheStatusRequest = (
  requestId: number,
  workspaceRoot: string,
): CacheStatusRequest => ({
  type: "cache_status",
  version: protocolVersion,
  request_id: requestId,
  workspace_root: workspaceRoot,
});

export const cacheCleanupRequest = (requestId: number): CacheCleanupRequest => ({
  type: "cache_cleanup",
  version: protocolVersion,
  request_id: requestId,
});

export const cacheListRequest = (requestId: number): CacheListRequest => ({
  type: "cache_list",
  version: protocolVersion,
  request_id: requestId,
});

export const cacheRemoveCurrentProjectRequest = (
  requestId: number,
  workspaceRoot: string,
): CacheRemoveRequest => ({
  type: "cache_remove",
  version: protocolVersion,
  request_id: requestId,
  scope: "current_project",
  workspace_root: workspaceRoot,
});

export const cacheRemoveSelectedRequest = (
  requestId: number,
  shardIds: readonly string[],
): CacheRemoveRequest => ({
  type: "cache_remove",
  version: protocolVersion,
  request_id: requestId,
  scope: "selected",
  shard_ids: [...shardIds],
});

export const cacheRemoveAllRequest = (requestId: number): CacheRemoveRequest => ({
  type: "cache_remove",
  version: protocolVersion,
  request_id: requestId,
  scope: "all",
});

export const cachePurgeOrphansRequest = (requestId: number): CacheRemoveRequest => ({
  type: "cache_remove",
  version: protocolVersion,
  request_id: requestId,
  scope: "orphans",
});
