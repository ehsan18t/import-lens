import assert from "node:assert/strict";
import test from "node:test";
import type { CacheListResponse, CacheStatusResponse } from "../../src/ipc/protocol.js";
import { cacheManagerActionItems, cacheShardPickItems } from "../../src/ui/cacheManagerItems.js";

test("cacheManagerActionItems presents summary and maintenance actions", () => {
  const items = cacheManagerActionItems(status());

  assert.equal(items[0]?.label, "ImportLens cache");
  assert.equal(items[0]?.description, "142 MB across 3 projects - limit 512 MB - 30 days");
  assert.deepEqual(
    items.slice(1).map((item) => item.action),
    ["cleanup", "clearCurrent", "clearAll", "inspect"],
  );
});

test("cacheShardPickItems maps project shards to readable items", () => {
  const items = cacheShardPickItems({
    version: 6,
    request_id: 2,
    shards: [
      {
        shard_id: "v1-abc",
        project_root: "C:/workspace/app",
        normalized_root: "c:/workspace/app",
        cache_path: "C:/Code/User/workspaceStorage/importlens/daemon-cache/v1-abc/cache.redb",
        size_bytes: 2048,
        last_used_millis: 1_789_000_000_000,
        loaded: true,
      },
    ],
    error: null,
    diagnostics: [],
  } satisfies CacheListResponse);

  assert.deepEqual(items, [
    {
      label: "C:/workspace/app",
      description: "2 kB",
      detail: "C:/Code/User/workspaceStorage/importlens/daemon-cache/v1-abc/cache.redb",
      shardId: "v1-abc",
    },
  ]);
});

const status = (): CacheStatusResponse => ({
  version: 6,
  request_id: 1,
  total_size_bytes: 142 * 1024 * 1024,
  project_count: 3,
  max_size_mb: 512,
  max_age_days: 30,
  last_cleanup_millis: null,
  current_project: null,
  error: null,
  diagnostics: [],
});
