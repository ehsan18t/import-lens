import assert from "node:assert/strict";
import test from "node:test";
import type { CacheStatusResponse } from "../../src/ipc/protocol.js";
import {
  cacheManagerActionItems,
  cacheRemovalToast,
  formatCacheBytes,
} from "../../src/ui/cacheManagerItems.js";

const NOW = 1_700_000_000_000;

const status = (overrides: Partial<CacheStatusResponse> = {}): CacheStatusResponse => ({
  version: 7,
  request_id: 1,
  total_size_bytes: 142 * 1024 * 1024,
  project_count: 3,
  max_size_mb: 512,
  current_project: null,
  total_bytes: 142 * 1024 * 1024,
  budget_bytes: 512 * 1024 * 1024,
  registry_size_bytes: 512 * 1024,
  error: null,
  diagnostics: [],
  ...overrides,
});

test("cacheManagerActionItems lists the scoped clears after read-only status (§8, RB-17)", () => {
  const items = cacheManagerActionItems(status(), NOW);

  assert.deepEqual(
    items.map((item) => item.action),
    [
      "summary",
      "summary",
      "clearCurrent",
      "clearAllProjects",
      "clearOrphans",
      "clearRegistry",
      "clearEverything",
    ],
  );

  // The retired Run-Cleanup / Inspect maintenance surface stays gone (X-22); the
  // orphan reclaim is a first-class action again (RB-17).
  const actions = new Set<string>(items.map((item) => item.action));
  assert.equal(actions.has("cleanup"), false);
  assert.equal(actions.has("inspect"), false);
  assert.equal(actions.has("clearAll"), false);
  assert.equal(actions.has("clearOrphans"), true);
  const orphans = items.find((item) => item.action === "clearOrphans");
  assert.match(orphans?.label ?? "", /orphan/i);
});

test("cacheManagerActionItems renders the E-status observability fields", () => {
  const items = cacheManagerActionItems(
    status({
      current_project: {
        shard_id: "v1-abc",
        project_root: "C:/workspace/app",
        normalized_root: "c:/workspace/app",
        cache_path: "C:/cache/v1-abc/cache.redb",
        size_bytes: 2048,
        last_used_millis: NOW - 2 * 86_400_000,
        loaded: true,
        entry_count: 5,
      },
    }),
    NOW,
  );

  const [usage, current] = items;
  // Total size vs budget.
  assert.equal(usage?.description, "142 MB / 512 MB");
  // Headroom + project count + registry size.
  assert.equal(usage?.detail, "370 MB free - 3 projects - registry 512 kB");
  // Per-project size + entry count + last used.
  assert.equal(current?.description, "2 kB - 5 entries");
  assert.equal(current?.detail, "Last used 2d ago");
});

test("cacheManagerActionItems falls back when the daemon omits E-status fields", () => {
  const items = cacheManagerActionItems(
    status({ total_bytes: undefined, budget_bytes: undefined, registry_size_bytes: undefined }),
    NOW,
  );

  assert.equal(items[0]?.description, "142 MB / 512 MB");
  assert.equal(items[0]?.detail, "370 MB free - 3 projects - registry 0 B");
});

test("cacheManagerActionItems shows a placeholder when the current project is uncached", () => {
  const items = cacheManagerActionItems(status({ current_project: null }), NOW);

  assert.equal(items[1]?.description, "Not cached yet");
  assert.equal(items[2]?.description, "No cache yet");
});

test("cacheRemovalToast reports accurate, non-overclaiming copy per scope (X-23/X-24)", () => {
  // Registry-only clear must never claim shard removals.
  assert.equal(
    cacheRemovalToast("registry", 0, 0),
    "Cleared Import Lens registry metadata (npm hints).",
  );
  assert.equal(
    cacheRemovalToast("currentProject", 1, 0),
    "Cleared the current project's Import Lens cache.",
  );
  assert.equal(
    cacheRemovalToast("currentProject", 0, 0),
    "No Import Lens cache to clear for the current project.",
  );
  assert.equal(
    cacheRemovalToast("allProjects", 3, 0),
    "Cleared 3 Import Lens caches across all projects.",
  );
  // "Everything" states everything it cleared, not just the shards.
  assert.equal(
    cacheRemovalToast("everything", 3, 0),
    "Cleared everything: 3 Import Lens caches plus registry metadata and derived caches.",
  );
  // Partial failure surfaces both counts honestly.
  assert.equal(
    cacheRemovalToast("allProjects", 2, 1),
    "Removed 2 Import Lens caches across all projects; 1 could not be removed.",
  );
  assert.equal(
    cacheRemovalToast("currentProject", 1, 0),
    "Cleared the current project's Import Lens cache.",
  );
  // Orphan reclaim (RB-17): reports what it reclaimed, and says so plainly when
  // nothing was orphaned.
  assert.equal(
    cacheRemovalToast("orphans", 2, 0),
    "Reclaimed 2 Import Lens caches for moved or deleted projects.",
  );
  assert.equal(
    cacheRemovalToast("orphans", 0, 0),
    "No orphaned Import Lens caches to reclaim, and nothing stale to scrub.",
  );
  assert.equal(
    cacheRemovalToast("orphans", 1, 1),
    "Reclaimed 1 Import Lens cache for moved or deleted projects; 1 could not be removed.",
  );
});

// The purge removes orphaned shards AND scrubs the shards it keeps AND prunes registry metadata.
// Reporting only the first meant a run that dropped hundreds of entries announced "nothing to
// reclaim" — a zero shown for work that happened, on the surface that asked for consent to do it.
test("an orphan purge that removed no shard still reports what it scrubbed", () => {
  assert.equal(
    cacheRemovalToast("orphans", 0, 0, 142, 7),
    "No orphaned Import Lens caches to reclaim; scrubbed 142 stale entries and 7 registry entries from the caches that remain.",
  );
  assert.equal(
    cacheRemovalToast("orphans", 0, 0, 1, 0),
    "No orphaned Import Lens caches to reclaim; scrubbed 1 stale entry from the caches that remain.",
  );
  assert.equal(
    cacheRemovalToast("orphans", 2, 0, 30, 0),
    "Reclaimed 2 Import Lens caches for moved or deleted projects, and scrubbed 30 stale entries from the caches that remain.",
  );
  // A genuinely empty run must still read as empty — the clause appears only when work happened.
  assert.equal(
    cacheRemovalToast("orphans", 0, 0, 0, 0),
    "No orphaned Import Lens caches to reclaim, and nothing stale to scrub.",
  );
});

test("formatCacheBytes scales units", () => {
  assert.equal(formatCacheBytes(512), "512 B");
  assert.equal(formatCacheBytes(2048), "2 kB");
  assert.equal(formatCacheBytes(142 * 1024 * 1024), "142 MB");
});
