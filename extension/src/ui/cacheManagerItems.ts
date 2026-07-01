import type {
  CacheListResponse,
  CacheShardInfo,
  CacheStatusResponse,
} from "../ipc/protocol.js";

export type CacheManagerAction = "summary" | "cleanup" | "clearCurrent" | "clearAll" | "inspect";

export interface CacheManagerActionItem {
  label: string;
  description?: string;
  detail?: string;
  action: CacheManagerAction;
}

export interface CacheShardPickItem {
  label: string;
  description: string;
  detail: string;
  shardId: string;
}

export const formatCacheBytes = (bytes: number): string => {
  if (bytes < 1024) {
    return `${bytes} B`;
  }

  const kilobytes = bytes / 1024;

  if (kilobytes < 1024) {
    return `${Math.round(kilobytes)} kB`;
  }

  const megabytes = kilobytes / 1024;

  if (megabytes < 1024) {
    return `${Math.round(megabytes)} MB`;
  }

  return `${(megabytes / 1024).toFixed(1)} GB`;
};

export const cacheManagerActionItems = (status: CacheStatusResponse): CacheManagerActionItem[] => [
  {
    label: "ImportLens cache",
    description: `${formatCacheBytes(status.total_size_bytes)} across ${status.project_count} projects - limit ${status.max_size_mb} MB - ${status.max_age_days} days`,
    detail: status.current_project
      ? `Current project: ${formatCacheBytes(status.current_project.size_bytes)}`
      : "No cache has been created for the current project yet.",
    action: "summary",
  },
  {
    label: "$(sync) Run Cleanup Now",
    description: "Remove expired and oversized project cache shards",
    action: "cleanup",
  },
  {
    label: "$(trash) Clear Current Project Cache",
    description: status.current_project
      ? formatCacheBytes(status.current_project.size_bytes)
      : "No current project cache",
    action: "clearCurrent",
  },
  {
    label: "$(trash) Clear All ImportLens Cache",
    description: formatCacheBytes(status.total_size_bytes),
    action: "clearAll",
  },
  {
    label: "$(folder-opened) Inspect Project Caches",
    description: `${status.project_count} project${status.project_count === 1 ? "" : "s"}`,
    action: "inspect",
  },
];

export const cacheShardPickItems = (response: CacheListResponse): CacheShardPickItem[] =>
  response.shards
    .slice()
    .sort(compareCacheShards)
    .map((shard) => ({
      label: shard.project_root,
      description: formatCacheBytes(shard.size_bytes),
      detail: shard.cache_path,
      shardId: shard.shard_id,
    }));

const compareCacheShards = (left: CacheShardInfo, right: CacheShardInfo): number =>
  right.size_bytes - left.size_bytes || left.project_root.localeCompare(right.project_root);
