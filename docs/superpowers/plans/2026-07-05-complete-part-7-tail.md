# Complete Part 7 Tail Implementation Plan

> **For agentic workers:** Use superpowers:subagent-driven-development or executing-plans. Checkbox steps.

**Goal:** Deliver the Part 7 items that were planned/claimed but dropped or left incomplete on the caching-hardening branch, so no commit message or plan claim is left unbacked by code. Four logical commits, each with tests.

**Gaps being closed (all verified absent in `git diff main..HEAD`):**
1. **Registry retention prune** — `92ef62b`'s message claims it's "handled by the orphan-purge action"; nothing prunes `RegistryMetadataCache`.
2. **L1 + GRAPH_CACHE path-purge** — the orphan purge only clears L1/graph when a whole *shard* is removed; entry-only purges (uninstalled package, project still present) leave L1/graph entries for gone packages.
3. **Disk entry cap** — no automatic bound on disk-entry count; only the orphan purge (user-triggered) + shard cleanup bound disk today.
4. **Purge-path tests** — the shipped `DiskCache::purge_orphan_entries` (redb scan/remove + stale-version branch) and `ProjectCacheRegistry::purge_orphans` shard removal ship untested (the one test uses disk-disabled `ImportCache`).

## Global constraints

- One squashed commit per item (C1–C4). C1's body notes it completes what `92ef62b` deferred.
- `cargo fmt`; `cargo clippy --workspace --all-targets` (no new warnings); `cargo test -p import-lens-daemon` green before each commit.
- Do NOT prune the registry on automatic load/persist (breaks the synthetic-timestamp persistence tests). Prune only on the user-triggered orphan purge.

---

## C1 — Registry retention prune

**Files:** `daemon/src/registry/constants.rs`, `daemon/src/registry/cache.rs`, `daemon/src/registry/service.rs`, `daemon/src/service.rs`; test in `daemon/tests/registry.rs` (or a `#[cfg(test)]` mod in cache.rs).

- [ ] Add `pub const REGISTRY_RETENTION_MS: u64 = 30*24*60*60*1000` to constants.rs.
- [ ] In cache.rs: refactor `persist_latest_snapshot` to `persist_snapshot(&self, prune_older_than: Option<(u64,u64)>)` — `None` = current behavior; `Some((now,retention))` filters the post-union snapshot before writing (stops the union resurrecting pruned rows). Keep `flush`/existing callers on `None`.
- [ ] Add pure `fn prune_expired_entries(&mut map, now, retention) -> usize` (retain fresh) + `pub fn purge_expired(&self, now_ms, retention_ms) -> usize` (prune in-memory + `persist_snapshot(Some(..))`).
- [ ] `RegistryHintService::purge_expired_metadata() -> usize` delegating (no-op for disabled service; uses `crate::time::unix_millis_now()` + the const).
- [ ] Wire into `service.rs` `remove_cache` `CacheRemoveScope::Orphans` arm: call `self.registry_hints.purge_expired_metadata()` (log count) then return `self.cache_registry.purge_orphans()`.
- [ ] Test (realistic timestamps): seed fresh + stale, `purge_expired` drops only stale and count==1, and a reload does NOT resurrect the stale entry. Confirm the two existing persistence tests still pass.

## C2 — L1 + GRAPH_CACHE path-purge on the orphan action

**Files:** `daemon/src/pipeline/file_size_cache.rs`, `daemon/src/pipeline/graph.rs`, `daemon/src/service.rs`; tests in-file.

- [ ] `FileSizeCache::purge_missing_paths(&self) -> usize` — drop entries whose document `Path::exists()` is false (iterate keys, collect missing, remove). Unit test with a real temp file (present → kept; removed → dropped).
- [ ] `graph.rs`: `pub fn purge_missing_module_graphs() -> usize` — drop `GRAPH_CACHE` entries whose entry-path key no longer exists on disk (guard `GRAPH_CACHE.get()`; iterate, collect keys where `!key.0.exists()`, remove). Unit test.
- [ ] In `service.rs` `remove_cache`, for the `Orphans` scope specifically (even when `removed.is_empty()`), call `shared_file_size_cache().purge_missing_paths()` + `purge_missing_module_graphs()`. Keep the existing `!removed.is_empty()` blanket clear for the other scopes. (Restructure so the orphan path always runs the path-selective purge.)

## C3 — Disk entry cap on flush

**Files:** `daemon/src/cache/disk.rs`; test in `daemon/tests/cache_disk.rs`.

- [ ] Add `const MAX_DISK_ENTRIES: usize` (generous, e.g. 20_000). In `write_pending_inserts` (inside the write txn, after the insert loop): if `table.len() > MAX_DISK_ENTRIES`, collect `(recents_ts, key)` for all rows, sort ascending, remove the oldest `len-cap` from both `CACHE_TABLE` and `RECENTS_TABLE`. Also trim dangling `recents` beyond the cap.
- [ ] Test: to keep it fast, expose the cap or add a test-only smaller-cap constructor, OR assert the eviction logic via a focused helper. Prefer: make the cap a `pub const` and, if a 20k-insert test is too slow, add a lower-level unit test that drives the eviction path with a crafted table (document the choice). At minimum assert count stays ≤ cap after inserting cap+N with disk enabled.

## C4 — Purge-path tests for the shipped orphan purge

**Files:** `daemon/tests/cache_disk.rs`, `daemon/tests/project_cache.rs`.

- [ ] `cache_disk.rs`: disk-ENABLED test seeding (a) a live entry (paths exist), (b) a missing-path entry, (c) a stale-`analyzer_version` entry; `purge_orphan_entries(ANALYZER_VERSION)` removes (b) and (c), keeps (a); assert via `DiskCache::get`/count.
- [ ] `project_cache.rs`: `purge_orphans` removes a shard whose `project_root` was deleted and keeps one whose root exists; assert via `list_shards`/results.

---

## Part gate (after all four)

- [ ] `cargo fmt` · `cargo clippy --workspace --all-targets` (no new warnings) · full daemon + extension suites green.
- [ ] Independent review of C1 (persist/union prune resurrection-safety) and C3 (redb eviction txn).
- [ ] Four clean commits; C1 body notes it completes `92ef62b`'s deferred registry work.
