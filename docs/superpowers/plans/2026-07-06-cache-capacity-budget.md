# Cache Capacity — Single Byte Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax. **Before executing, run a fresh planning-brief + verification pass** (as done for Plans 1–2): this plan predates execution; the exact `file:line` anchors must be re-confirmed against HEAD, and the cross-shard evictor is intricate.

**Goal:** Replace the ~5 overlapping bounds (512 MB whole-shard cap, 4096 memory-entry cap's wall-clock LRU, 20k disk-entry count cap, 200k recycle, whole-shard age cleanup, and the two-queue `recents` design) with **one global disk-byte budget** and entry-granular LRU eviction driven by a single monotonic recency sequence, plus threshold-triggered redb compaction so the on-disk footprint actually tracks the budget.

**Architecture:** Plan 4 of the redesign (spec §5), on top of Plans 1–3. Recency becomes a process-global monotonic `u64` sequence stamped on the entry (killing the wall-clock ties, clock-jump bugs, and the dangling-`recents`-row family in one move). Each project shard keeps an in-memory rollup `{total_bytes, oldest_seq, entry_count}` (rebuilt by a one-time scan at load). A single `BudgetCoordinator` sums the rollups; when total bytes exceed `cacheMaxSizeMB`, it repeatedly picks the shard with the smallest `oldest_seq`, scans just that shard for its lowest-seq entries, and evicts them down to a low-water mark — exact-enough global LRU without a persisted cross-shard index (redb 4.1.0 has no secondary-index type). A per-project floor keeps each project's newest N entries; bulk/prewarm reads don't promote recency (scan resistance). A `Compactor` reclaims redb free pages when fragmentation crosses a threshold.

**Tech Stack:** Rust (daemon: redb 4.1.0, papaya, tokio+rayon). No new deps.

> **HEAD re-anchor (validated 2026-07-06 against `cf9945c`, branch `redesign/cache-lifecycle`).** Plan is executable and the architecture fits; corrections to apply before executing:
> - **Task 1 premise fix:** `CacheEnvelope` (`disk.rs:43`) does **NOT** currently persist recency at all — it holds only `analyzer_version, result, dependency_fingerprints, full_contributions`. Disk recency lives in a **separate `RECENTS_TABLE`** (`TableDefinition<&str, u64>`, `disk.rs:21`) as unix-millis via the touch queue. So Task 1 **adds** `last_seq` as a net-new `CacheEnvelope` field (and deletes `RECENTS_TABLE` in Task 3) — it does not "replace `last_used_millis`" in the envelope. `CachedImport` *does* still carry `last_used_millis: Arc<AtomicU64>` (`memory.rs:64`), which Task 1 replaces with `last_seq`; wall-clock LRU is at `enforce_memory_cap` `min_by_key(last_used_millis)` (`memory.rs:252`).
> - **Deletion-inventory name drift (Task 3):** the service-layer wrapper is **`flush_cache_recency_touches`** (`service.rs:1174`), not `flush_recency_touches`. `pending_recency_touch_count` exists **only** in `memory.rs:321`. `flush_recency_touches` exists at `memory.rs:325` + `project.rs:362`. Use these exact names when grepping the deletion set.
> - **Task 6 `&mut Database` restructure is MANDATORY, not optional.** `DiskCache.db` is a bare `Option<Database>` (`disk.rs:52`) with no interior mutability, and `ImportCache`/`DiskCache` are always shared as `Arc<ImportCache>` (`LoadedProjectCache.cache: Arc<ImportCache>`) — there is **no `&mut` path** to the `Database` through the Arc. Compaction requires making `db` a `Mutex<Option<Database>>` (or reopen-for-compaction). Confirm `WriteTransaction::stats()` field names (`fragmented_bytes`, `allocated_pages`) against redb 4.1.0 before coding.
> - **Cross-shard evictor — feasible, with two frictions to design for (Task 5):** (1) `ProjectCacheRegistry.loaded` (`project.rs:25`) tracks only **currently-loaded** shards; unloaded shards live on disk (`scan_disk_shards`). The coordinator must enumerate/open unloaded shards to evict them, or persist a per-shard summary. (2) The design §5.2 "cheap stored per-shard summary / by-`last_seq` secondary index" **does not exist yet** — there is no stored `total_bytes`/`oldest_seq` and no seq index, so `shard_rollup()` (Task 4) is a **full `CACHE_TABLE` scan per shard** at startup (O(all entries)), not the O(1)-per-shard the design implies. Not a blocker; property-test the "pick smallest `oldest_seq` → scan → evict → recompute" loop as the plan asks. Single-writer-per-redb is **per-shard** (each shard its own `Database`), so cross-shard eviction has no global write-lock contention.
> - **`size_bytes` (Tasks 2/4): prefer derived, no envelope field.** `CACHE_TABLE` value type is `&[u8]` (`disk.rs:20`), so `value.value().len()` is the exact on-disk entry size — use it for the rollup; keep `CachedImport.size_bytes` as an insert-time convenience only. Do **not** add a self-referential `CacheEnvelope.size_bytes` (its value would depend on serializing itself). This confirms the Task 2 reviewer note.

## Global Constraints

- **Conventional Commits, mandatory body, header ≤ 72 chars.**
- **Gates:** `cargo clippy --workspace --all-targets` (deny), `cargo deny check`, `cargo fmt`, `cargo test -p import-lens-daemon`. Trust `cargo check`, not the stale rust-analyzer.
- **`CacheEnvelope` gains `last_seq`** (`#[serde(default)]` so old-format entries decode; a schema bump would wipe — avoid unless intended; Plan 2 already set schema 5). It does **not** gain `size_bytes` — on-disk size is derived from the `CACHE_TABLE` value length (`&[u8]`), never a self-referential envelope field (see HEAD re-anchor note).
- **The budget is measured in logical entry bytes** (summed `size_bytes`); actual `.redb` file size is reconciled by the `Compactor` (redb reuses freed pages rather than shrinking).
- **Eviction never loses data the byte budget should keep:** the memory mirror stays a working-set cache of disk; evicting from memory only sheds the mirror. Disk eviction is the real budget enforcement.
- **`redb::Database::compact` needs `&mut Database` and NO live read transaction** — the `Compactor` must run when the shard is idle (no in-flight reads) and hold `&mut` (restructure `DiskCache.db` access or reopen).
- **Setting:** `importLens.cacheMaxSizeMB` (default 512) is now the **global total** byte budget; `importLens.cacheMaxAgeDays` is **deprecated** (age cleanup removed) — ignore gracefully, mark deprecated in `package.json`.

## File Structure

- `daemon/src/cache/recency.rs` (new) — `RecencyClock` (global `AtomicU64`) + `next_seq()`.
- `daemon/src/cache/memory.rs` — `CachedImport.last_seq`/`size_bytes` replace `last_used_millis`; interactive-vs-bulk get (`promote`); working-set cap keyed on `last_seq`.
- `daemon/src/cache/disk.rs` — delete `RECENTS_TABLE` + `pending_touches` two-queue + `MAX_DISK_ENTRIES` cap; persist `last_seq` in `CacheEnvelope` (size is **derived** from the `CACHE_TABLE` value length, not stored); a `shard_rollup()` scan + a `lowest_seq_keys(n)` scan + a `remove_keys(&[key])` evictor primitive; a `compact_if_fragmented()`.
- `daemon/src/cache/budget.rs` (new) — `BudgetCoordinator`: per-shard rollups + `record_insert`/`record_remove`, `evict_to_budget()`, per-project floor.
- `daemon/src/cache/project.rs` — delete the whole-shard age + size `cleanup` branches; wire the coordinator into load/insert; rollups per `LoadedProjectCache`.
- `daemon/src/lifecycle.rs` + `daemon/src/ipc/server.rs` — delete the `CACHE_RECYCLE_ENTRY_LIMIT` branch + `cache_len` recycle plumbing.
- `daemon/src/service.rs` + `daemon/src/ipc/protocol.rs` — `cacheMaxSizeMB` → byte budget; deprecate `cacheMaxAgeDays`.

---

## Task 1: Monotonic recency sequence on the entry

**Files:** Create `daemon/src/cache/recency.rs`; Modify `daemon/src/cache/memory.rs`, `daemon/src/cache/disk.rs` (`CacheEnvelope`, `decode_cached_result`); Test `daemon/tests/freshness_core.rs` / `memory_cache.rs`.

**Interfaces:**
- `pub struct RecencyClock;` with `pub fn next_seq() -> u64` (a `static AtomicU64`, `fetch_add(1, Relaxed)`, starting at 1).
- `CachedImport.last_seq: u64` and `CacheEnvelope.last_seq: u64` (`#[serde(default)]`) **replace** `last_used_millis`. `get(promote: bool)`: interactive gets call `next_seq()` and bump `last_seq`; bulk/prewarm gets pass `promote=false` (scan resistance).

- [ ] **Step 1: Failing test** — `next_seq()` is strictly increasing; two interactive `get`s on the same key produce an increasing `last_seq`; a `promote=false` get does NOT change `last_seq`.
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Add `recency.rs` + `pub mod recency` in `cache/mod.rs`. Replace `CachedImport.last_used_millis: Arc<AtomicU64>` with `last_seq: u64` (plain — no Arc; recency now updates via re-insert of the small struct or an in-place seq field). Add `last_seq` to `CacheEnvelope` (`#[serde(default)]`) and set it in `cache_envelope` + read it in `decode_cached_result` (default `0` for legacy rows).
- [ ] **Step 4:** Thread a `promote: bool` through `ImportCache::get` (public `get` = interactive `promote=true`; add a `get_for_prewarm`/`promote=false` used by preload). On an interactive hit, set `last_seq = RecencyClock::next_seq()` (re-insert the entry to persist the bump in memory; the disk copy's `last_seq` is refreshed on the next flush). Update `enforce_memory_cap` to evict by `min_by_key(last_seq)` instead of `last_used_millis`.
- [ ] **Step 5:** Compile the crate (many call sites of `last_used_millis` — grep + fix, e.g. `decode_cached_result`, tests). Run tests green; clippy.
- [ ] **Step 6:** Commit `feat(daemon): monotonic recency sequence on cache entries`.

---

## Task 2: Per-entry byte size

**Files:** Modify `daemon/src/cache/disk.rs` (capture `bytes.len()` at insert; `CacheEnvelope.size_bytes`), `daemon/src/cache/memory.rs` (`CachedImport.size_bytes`). Test: unit.

**Interfaces:**
- `CachedImport.size_bytes: u64`, `CacheEnvelope.size_bytes: u64` (`#[serde(default)]`) — the serialized envelope byte length, captured for free at `disk.rs` insert (`let bytes = rmp_serde::to_vec(&envelope)?; let size_bytes = bytes.len() as u64;`).

- [ ] **Step 1: Failing test** — after inserting an entry, its persisted `size_bytes` equals the serialized envelope length (read back the envelope, assert `size_bytes == encoded.len()` within the self-describing tolerance, i.e. capture size after encoding).
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Add `size_bytes` to both structs. In `DiskCache::insert`, after `rmp_serde::to_vec(&envelope)`, note that `size_bytes` must be embedded *inside* the envelope but the envelope's own encoded length changes when you add the field — capture it as "the encoded length of the value stored in `CACHE_TABLE`" (i.e. `bytes.len()` of the final serialized form) and store that number in the in-memory rollup, not necessarily round-tripped identically. Decision: store `size_bytes` on the in-memory `CachedImport` from `bytes.len()` at insert, and let the rollup use `value.value().len()` when scanning disk (`CACHE_TABLE` value length is the true on-disk entry size) — so `CacheEnvelope` may not need `size_bytes` at all; the on-disk size is `value.value().len()`. Prefer deriving size from the stored value length (no envelope field) to avoid the self-referential encoding problem.
- [ ] **Step 4:** Tests green; clippy.
- [ ] **Step 5:** Commit `feat(daemon): track per-entry serialized byte size`.

> **Reviewer note (execution):** resolve whether `size_bytes` is an envelope field or derived from `CACHE_TABLE` value length. Deriving from `value.value().len()` on scan is simpler and exact; the in-memory `CachedImport.size_bytes` is a convenience for the coordinator on insert. Do NOT store a size field whose value depends on serializing itself.

---

## Task 3: Delete the two-queue recents machinery + disk count cap

**Files:** Modify `daemon/src/cache/disk.rs` (remove `RECENTS_TABLE`, `pending_touches`, `touch`/`flush_recency_touches`/`write_pending_touches`/`merge_pending_touches`/`remove_pending_touch`/`clear_pending_touches`, `MAX_DISK_ENTRIES` + its eviction block, `RECENCY_TOUCH_FLUSH_BATCH`); `recent_keys` reimplemented as a `lowest`/`highest`-by-`last_seq` scan of `CACHE_TABLE`; `daemon/src/cache/memory.rs` + `project.rs` + `service.rs` wrappers (`pending_recency_touch_count`, `flush_recency_touches`). Test: replace `write_pending_inserts_bounds_disk_entry_count`.

**Interfaces:**
- Recency is now the entry's `last_seq`; there is no separate recents table or touch queue. `recent_keys(limit)` (for prewarm) returns the `limit` HIGHEST-`last_seq` keys by scanning `CACHE_TABLE` and decoding each envelope's `last_seq` (or reading it cheaply). `remove`/`invalidate`/`purge_orphan_entries` no longer touch a recents table (removing the entry removes its recency — no dangling rows possible).

- [ ] **Step 1: Failing test** — after inserting N entries with increasing `last_seq`, `recent_keys(k)` returns the k highest-`last_seq` keys; after `remove(key)`, no orphan recency state remains (assert via a full scan that no key lacks a live `CACHE_TABLE` row). Delete/replace `write_pending_inserts_bounds_disk_entry_count` and the recents-timestamp tests.
- [ ] **Step 2:** Run — FAIL / compile errors from removed symbols.
- [ ] **Step 3:** Remove the `RECENTS_TABLE` def + all its I/O, the `pending_touches` field + all touch machinery, `MAX_DISK_ENTRIES` + its eviction block in `write_pending_inserts` (eviction moves to the coordinator, Task 5). Reimplement `recent_keys` via a `CACHE_TABLE` scan decoding `last_seq`. Update `Drop` (only `flush_pending_inserts`). Fix `ImportCache`/registry/service wrappers that referenced the touch API (grep the deletion inventory: `pending_recency_touch_count`, `flush_recency_touches`, `flush_cache_recency_touches`).
- [ ] **Step 4:** `cargo check` green (this is a wide deletion — let the compiler drive); tests green; clippy.
- [ ] **Step 5:** Commit `refactor(daemon): replace the recents two-queue with entry seq`.

---

## Task 4: Per-shard rollup

**Files:** Modify `daemon/src/cache/disk.rs` (`shard_rollup()` scan), `daemon/src/cache/project.rs` (rollup per `LoadedProjectCache`, built at load). Test: `daemon/tests/cache_disk.rs`.

**Interfaces:**
- `pub struct ShardRollup { pub total_bytes: u64, pub oldest_seq: u64, pub entry_count: u64 }`.
- `DiskCache::shard_rollup(&self) -> ShardRollup` — one scan of `CACHE_TABLE`, summing `value.value().len()` (size) and tracking `min(last_seq)` + count. Called at shard load; maintained incrementally by the coordinator on insert/evict (Task 5) so it isn't rescanned per operation.

- [ ] **Step 1: Failing test** — insert entries of known sizes with known seqs; `shard_rollup()` returns the correct `total_bytes`, `oldest_seq` (min), `entry_count`.
- [ ] **Step 2–4:** Implement the scan; build the rollup when a shard is loaded (`cache_for_root`) and store it on `LoadedProjectCache`; test green; clippy.
- [ ] **Step 5:** Commit `feat(daemon): per-shard size/recency rollup`.

---

## Task 5: BudgetCoordinator + global byte-budget evictor

**Files:** Create `daemon/src/cache/budget.rs`; Modify `daemon/src/cache/project.rs` (own the coordinator; wire into insert-flush + startup), `disk.rs` (`lowest_seq_keys(n)` + `remove_keys`), delete the whole-shard age/size `cleanup` branches. Test: `daemon/tests/` new byte-budget test.

**Interfaces:**
- `BudgetCoordinator { budget_bytes: u64, rollups: Mutex<HashMap<ShardId, ShardRollup>> }` with `record_insert(shard, delta_bytes, seq)`, `record_remove(shard, delta_bytes)`, `total_bytes()`, and `evict_to_budget(&registry)`.
- `evict_to_budget`: while `total_bytes() > budget`, pick the shard with the smallest `oldest_seq`; `lowest_seq_keys(shard, batch)` skipping that shard's newest `FLOOR` entries (per-project floor); `remove_keys` them; update the rollup (recompute `oldest_seq`); stop at `budget * LOW_WATER` (e.g. 0.9) to avoid thrash.
- `DiskCache::lowest_seq_keys(n, floor)` — scan `CACHE_TABLE`, return the `n` lowest-`last_seq` keys excluding the shard's `floor` highest. `DiskCache::remove_keys(&[String])` — batch delete + report freed bytes.

- [ ] **Step 1: Failing test** — set a small budget; insert entries across ≥2 shards totalling over budget; after `evict_to_budget`, total logical bytes ≤ `budget*LOW_WATER`, the globally-lowest-seq entries are gone first, and each shard's most-recent `FLOOR` entries survive (per-project floor). A single active shard that exceeds the budget on its own DOES get trimmed (unlike the old whole-shard cap).
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Implement `budget.rs`; wire `record_insert`/`record_remove` into the disk insert-flush + eviction paths and `evict_to_budget` after each flush + at startup. Delete the whole-shard age + size branches in `project.rs::cleanup` (the coordinator subsumes them; keep the `cleanup` entry point as a manual "evict now").
- [ ] **Step 4:** `cargo test` green; clippy. Confirm the coordinator is the single writer for byte accounting (no races between the 4 insert paths — route rollup updates through it).
- [ ] **Step 5:** Commit `feat(daemon): global byte-budget LRU evictor`.

---

## Task 6: Compactor (reclaim redb free pages)

**Files:** Modify `daemon/src/cache/disk.rs` (`compact_if_fragmented`), wire from the coordinator/idle path. Test: `daemon/tests/cache_disk.rs`.

**Interfaces:**
- `DiskCache::compact_if_fragmented(&mut self, threshold: f64) -> bool` — read `WriteTransaction::stats()` (`fragmented_bytes` / `allocated_pages*page_size`); if free ratio > threshold and no live read txn, call `Database::compact` (`&mut Database`). Returns whether it compacted.
- Requires `&mut` access to the `Database` — restructure so the compactor can take it (e.g. `DiskCache` exposes a `&mut self` compaction entry called from an idle maintenance tick, not during serving).

- [ ] **Step 1: Failing test** — insert many entries, evict most (creating free pages), assert the `.redb` file size (or `allocated_pages*page_size`) shrinks after `compact_if_fragmented` vs before.
- [ ] **Step 2–4:** Implement; call it after large evictions and/or on an idle maintenance tick; test green (note: compaction requires no open read txn — the test must drop readers first); clippy.
- [ ] **Step 5:** Commit `feat(daemon): threshold-triggered redb compaction`.

---

## Task 7: Delete recycle; wire the byte-budget setting; deprecate age

**Files:** Modify `daemon/src/lifecycle.rs` (drop `CACHE_RECYCLE_ENTRY_LIMIT` + its `should_recycle` branch), `daemon/src/ipc/server.rs` (drop `cache_len` recycle plumbing — keep the idle-recycle branch), `daemon/src/service.rs` + `ipc/protocol.rs` (`cacheMaxSizeMB` → coordinator budget; `cacheMaxAgeDays` deprecated), `package.json` (mark `cacheMaxAgeDays` deprecated). Test: `daemon/tests/`.

- [ ] **Step 1: Failing test** — the coordinator's budget equals `cacheMaxSizeMB * 1024 * 1024` from the Hello policy; a Hello with a small `cacheMaxSizeMB` produces a small budget and triggers eviction. `should_recycle` no longer fires on entry count (only the idle/uptime branch remains).
- [ ] **Step 2:** Run — FAIL.
- [ ] **Step 3:** Remove `CACHE_RECYCLE_ENTRY_LIMIT` + the `cache_len > LIMIT` branch in `should_recycle`; drop the `cache_len` arg threading (`recycle_if_needed`, `service.cache_len`). Wire `cache_max_size_mb` into the `BudgetCoordinator` budget (via `new_with_cache_policy`). Keep `cache_max_age_days` accepted (ignored) + `#[serde(default)]`; mark deprecated in `package.json` config schema.
- [ ] **Step 4:** `cargo test` green; clippy; deny.
- [ ] **Step 5:** Commit `refactor(daemon): drop recycle, wire the byte budget`.

---

## Self-Review (against spec §5)

- §5.1 recency-in-entry monotonic seq + scan resistance → Task 1. §5.1 kill dangling recents → Task 3. ✅
- §5.2 per-shard rollup (I-2: index-in-shard, not a separate persisted global ledger) → Task 4. ✅
- §5.3 global evictor + hysteresis + per-project floor → Task 5. ✅
- §5.4 memory working-set bound (kept, re-pointed to `last_seq`) → Task 1. ✅
- §5.5 logical-bytes accounting + threshold compaction → Tasks 2 + 6. ✅
- §5.6 collapse: A1 age (Task 5 deletes cleanup branches), A2 size (Task 5), A4 disk count (Task 3), A7 recycle (Task 7). ✅
- **Open design decisions to lock at execution** (flagged inline): (1) `size_bytes` as an envelope field vs derived from `CACHE_TABLE` value length — prefer derived (Task 2 note); (2) how a memory-hit's `last_seq` bump reaches disk recency (flush-time persistence vs a lightweight seq write) — Task 1/3 choose flush-time; confirm it doesn't let the disk evictor drop a memory-hot entry (the per-project floor + memory residence mitigate); (3) `&mut Database` access for compaction (Task 6) — restructure `DiskCache.db` or reopen; (4) exact `FLOOR`/`LOW_WATER` defaults — tune empirically.
- **Cross-shard eviction is the intricate core (Task 5):** re-confirm the rollup-driven "pick smallest oldest_seq shard, scan it, evict, recompute" loop terminates and is exact-enough global LRU; property-test it. This plan was authored from a survey — do the planning-brief + verification pass before executing.
- **This plan touches the same disk.rs hot path as Plan 3's freshness code** — sequence Plan 3 before Plan 4 (or rebase carefully); the recency/eviction rewrite is large.
