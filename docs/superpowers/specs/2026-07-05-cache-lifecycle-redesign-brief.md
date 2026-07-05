# Cache Lifecycle Redesign — Context Brief

> **Purpose:** Hand-off doc for a fresh conversation. The daemon's cache lifecycle grew by accretion (several mechanisms bolted on incrementally) and now has ~7 overlapping eviction/clearing systems that don't compose cleanly. This brief captures each mechanism, how it misbehaves or conflicts, and the deferred items — so the redesign can start from a clean slate instead of another patch.
>
> **Direction already agreed:** the primary automatic bound should be a **single total disk-size budget** (LRU-evict when over), which should subsume most of the entry/shard/age mechanisms below. Everything here should be re-derived against that model, not preserved.
>
> All code is on `main` (caching-hardening work + follow-ups already merged). Read this doc + the cited files; do not rely on prior chat context.

---

## Part A — The overlapping automatic bounds (the core problem)

Four+ independent mechanisms try to bound the cache, at different scopes, on different triggers, with different units. It is unclear which one "wins" in any given situation.

### 1. Shard **age** cleanup — `max_age_days = 30`
- **What:** Removes an entire project's cache shard if its `last_used_millis` is older than 30 days.
- **Where:** `daemon/src/cache/project.rs` `cleanup()`; default set in `service.rs` (`new_with_cache_policy(.., 512, 30)`).
- **Trigger:** daemon startup (Hello) + explicit "Run Cleanup Now" button.
- **Issues:** Whole-shard granularity (all-or-nothing per project). Only runs at startup/explicit, never continuously. The "30 days" here means *shard last-used*, which is semantically different from the registry's "30 days" (item 5) — two same-numbered windows that mean different things.

### 2. Shard **size** cap — `max_size_mb = 512`
- **What:** When the total on-disk size of all shards exceeds 512 MB, removes whole shards oldest-first (by `last_used`) until under budget.
- **Where:** `project.rs` `cleanup()`.
- **Trigger:** startup + explicit cleanup only.
- **Issues:** Whole-shard granularity; the **active** project's shard is newest-used so it's evicted last — meaning a single active project can grow its own shard large without this ever trimming it. Only fires at startup/explicit. This is the closest thing to a "total size budget" today but it's coarse (shard-level) and rarely runs.

### 3. **Memory** entry cap — `MAX_MEMORY_ENTRIES = 4096` (per shard)
- **What:** Per-shard in-memory `ImportCache` LRU cap; evicts least-recently-used entry on insert when over 4096. Disk copy survives.
- **Where:** `daemon/src/cache/memory.rs` (`last_used_millis: Arc<AtomicU64>`, `enforce_memory_cap`).
- **Trigger:** every insert / disk re-hydrate.
- **Issues:** Added this session as a patch. Bounds only the in-memory mirror, not disk. O(n) `min_by_key` scan per insert at steady state. Overlaps conceptually with the disk cap (item 4) and recycle (item 7) with no clear ownership.

### 4. **Disk** entry cap — `MAX_DISK_ENTRIES = 20_000` (per shard)
- **What:** Per-shard on-disk (redb) entry cap; on the batched insert flush, evicts least-recently-used entries (by `recents` timestamp) when over 20k.
- **Where:** `daemon/src/cache/disk.rs` `write_pending_inserts`.
- **Trigger:** insert flush.
- **Issues:** Added this session as a patch. **Count-based, so disk *bytes* are unpredictable** (entry size varies widely) — directly at odds with a size budget. Interacts unclearly with the 512 MB shard cap (item 2): which bites first depends on average entry size. Does **not** trim the `recents` table itself (see item 8b).

### 5. **Registry** retention — `REGISTRY_RETENTION_MS = 30 days`
- **What:** Drops `registry-metadata.json` entries whose `updated_at` is older than 30 days, so the shared metadata file stops growing monotonically.
- **Where:** `daemon/src/registry/cache.rs` `purge_expired`; `registry/constants.rs`.
- **Trigger:** **only** the "Purge Orphan Cache" button (deliberately not on load/persist, to avoid breaking tests that use synthetic timestamps).
- **Issues:** Added this session. Separate 30-day window from item 1 (different meaning). Only user-triggered, so the file still grows between purges. Not wired to "Clear All" (see item 8a).

### 6. **Orphan purge** — user button ("Purge Orphan Cache")
- **What:** Drops genuinely-orphaned entries without a project scan: stale-`analyzer_version` entries (release churn), entries whose package `entry_path`/`package_root` no longer exist (uninstalled), whole shards whose `project_root` is gone, plus L1 file-size + module-graph entries for missing paths, plus registry retention (item 5).
- **Where:** `service.rs` `remove_cache` (`CacheRemoveScope::Orphans`); `cache/key.rs` `cache_key_is_orphan`; `disk.rs`/`memory.rs` `purge_orphan_entries`; `project.rs` `purge_orphans`; `pipeline/file_size_cache.rs` `purge_missing_paths`; `pipeline/graph.rs` `purge_missing_module_graphs`.
- **Trigger:** user button only.
- **Issues:** This is the most coherent piece, but it's *user-triggered only*. A lot of its job (dropping stale-version entries, uninstalled packages) arguably should be automatic and/or subsumed by a size-budget + validity model. Overlaps with items 1–5.

### 7. **Recycle** — `CACHE_RECYCLE_ENTRY_LIMIT = 200_000` (total entries)
- **What:** When total cached entry count exceeds 200k, the daemon flushes and recycles (rebuilds), clearing memory.
- **Where:** `daemon/src/lifecycle.rs` `should_recycle`; `ipc/server.rs` `recycle_if_needed`.
- **Trigger:** checked on message handling.
- **Issues:** Yet another count-based global bound (200k) that overlaps the per-shard memory cap (4096) and disk cap (20k). With the per-shard caps now in place, 200k is almost never reached — the mechanisms are redundant/unclear.

---

## Part B — The buttons (Cache Manager UI)

`extension/src/ui/cacheManagerItems.ts` presents five actions; their semantics overlap and one is inconsistent.

### 8a. **Clear All ImportLens Cache**
- **What:** Removes all project shards + clears module-graph cache + clears L1 file-size cache.
- **Where:** `service.rs` `remove_cache` (`CacheRemoveScope::All`).
- **Issue (confirmed gap):** Does **not** clear the registry metadata (item 5). There is no full registry `clear()` at all — only retention `purge_expired`. So "Clear All Cache" does not clear all caches. Inconsistent with the intent of Task 7.5 Step 2b (which added L1 clear to this path "so clearing refreshes everything").

### 8b. **Run Cleanup Now**
- **What:** Triggers the shard age + size cleanup (items 1 & 2) on demand.
- **Issue:** Overlaps "Clear All" and "Purge Orphans" conceptually; users won't know which button to use for what.

### 8c. **Clear Current Project Cache** — removes the current workspace's shard(s). Fine, but overlaps.

### 8d. **Purge Orphan Cache** — item 6.

### 8e. **Inspect Project Caches** — read-only listing. Fine.

### 8f. **`recents` table is unbounded** (confirmed gap)
- **What:** `RECENTS_TABLE` (redb) holds one recency row per key; used for preload + as the LRU signal for the disk cap.
- **Where:** `disk.rs` (`write_pending_touches` inserts unconditionally).
- **Issue:** Never independently bounded. A pending *touch* flushed after its key was evicted/invalidated re-creates a dangling recents row with no cache-table counterpart. These accumulate and are fully scanned by `recent_keys` (preload) and the disk-cap eviction. Plan C3 called for "trim dangling recents beyond the cap" — never implemented.

---

## Part C — Cross-cutting problems (summary)

- **No single owner of "how big can the cache get."** Size is governed by: 512 MB shard cap (coarse, rarely runs), 4096 memory entries/shard, 20k disk entries/shard, 200k total recycle — four answers, none authoritative, none continuous.
- **Count vs bytes mismatch.** Entry caps are count-based; the thing users care about (and the one budget knob) is bytes. Count caps make disk usage unpredictable.
- **Two "30 day" windows** with different meanings (shard last-used vs registry updated-at).
- **Automatic vs user-triggered is inconsistent.** Some cleanup is automatic (startup), some is buttons-only (orphan purge, registry prune); the split is arbitrary.
- **"Clear All" isn't all** (registry left behind); **`recents` isn't bounded.**
- **Buttons overlap** (Clear All / Cleanup / Purge Orphans / Clear Current) with unclear per-button purpose.

---

## Part D — Deferred issues (from the original caching-hardening plan; still open)

These were consciously deferred and are NOT yet implemented. Fold into the redesign where relevant.

### D1. Stale-while-revalidate (SWR) for bundle sizes
- **What it would do:** Serve the last-known bundle size (flagged "stale") while recomputing in the background, instead of delete-on-stale + a loading state.
- **Where it'd hook:** `memory.rs` `get()` currently deletes on fingerprint mismatch and returns `None`; SWR needs a non-evicting `get_with_freshness`, revalidation in the service layer, and an in-flight dedupe set.
- **Caveat:** Must distinguish "file changed" (serve stale + refresh) from "file gone" (delete — recompute can never succeed). Lower value than registry SWR (bundle recompute is local CPU, not network). The registry path already implements SWR (cached-then-background-refresh) and is the reference pattern.

### D2. Drop the fingerprint from the cache key (identity v3 → v4)
- **What it would do:** Remove `manifest_fingerprint` + `entry_fingerprint` from `CacheIdentityV3` (`cache/key.rs`) so an mtime-only change (`npm ci`, `git checkout`, reinstall with identical content) reuses the same key instead of minting a new one and orphaning the old. Rely on the value-side `dependency_fingerprints` re-validation for staleness (which already exists).
- **Why it matters:** In-key fingerprints are the biggest source of orphan accumulation (every reinstall orphans entries). This attacks orphans at the source and would reduce how much the orphan-purge/eviction machinery even needs to do.
- **Caveat:** Cache-identity version bump (v3→v4) — correctness-sensitive. Also the L1 file-size signature (`pipeline/file_size_cache.rs`) currently derives from this key, so it would need to fold fingerprints in independently.

### D3. 30s TTL staleness for workspace / linked (first-party) deps (LOW, original finding, un-addressed)
- **What:** `memory.rs` `REVERIFY_TTL_MS = 30s` fast-path skips the per-entry re-stat when verified within 30s at the current generation. `CACHE_GENERATION` bumps only on node_modules invalidation, so a `pnpm` workspace / `npm link` / `file:` dep edited directly (no watcher event) can serve a stale size for up to 30s.
- **Scope:** Only non-entry, non-manifest internal-module edits of such a dep (entry/manifest edits change the key). Niche; may or may not be worth addressing.

### D4. Concurrent spawned analysis can re-insert a stale entry post-invalidation (LOW, self-heals)
- **What:** A `tokio::spawn`'d report running when `NodeModulesChanged` arrives can `insert` after the generation bump, stamping the new generation → a read within 30s serves stale. Self-heals after the TTL. `server.rs` + `service.rs` `analyze_and_cache`.

---

## Part E — For the new conversation

**Agreed starting point:** a single **total disk-size budget** as the primary automatic bound (LRU-evict entries — not whole shards — until under budget). Re-derive everything in Parts A/B against that.

**Open design questions to resolve next (not yet decided):**
1. Budget scope: **global** (all projects share one N-MB budget) vs **per-project**? (Global is simpler and matches "max total cache size.")
2. What survives from Parts A/B: does the size budget replace the shard age cleanup (item 1), shard size cap (item 2), memory cap (item 3), disk cap (item 4), and recycle (item 7)? (Likely yes — collapse to one LRU-by-size evictor + a small memory working set.)
3. Buttons: collapse to a minimal set — likely **Clear All** (truly all, incl. registry), **Clear Current Project**, **Purge Orphans** (or fold orphan-validity into automatic eviction), **Inspect**. Drop "Run Cleanup Now" if eviction is continuous.
4. Registry: give it a real `clear()` (for Clear All) + keep retention; decide if it shares the size budget or stays a small separate file.
5. Validity vs capacity: separate "is this entry still valid?" (fingerprint/analyzer-version/path-exists) from "do we have room?" (size budget). Orphan detection becomes a validity concern that can run lazily on access + on a real clear, not a special button.
6. `recents`/LRU bookkeeping: one consistent recency source that can't leave dangling rows.
7. Deferred D1–D4: decide which fold into the new model (D2 in particular reduces orphan pressure).

**Do not** start implementing in the new conversation until this is brainstormed into an approved design + spec + plan.
