# Cache Lifecycle Redesign — Review Backlog

> **Source:** Multi-agent adversarial review of `redesign/cache-lifecycle` (`main...HEAD`, core commit `3038a15`) on 2026-07-07/08.
> **Method:** 13 finder angles → dedup → 8 clustered verifiers (every candidate judged CONFIRMED / PLAUSIBLE / REFUTED against the actual code, with quoted evidence) → gap sweep.
> **Tally:** 25 confirmed, 7 plausible, 12 refuted, plus unverified quality-debt candidates from the cleanup angles.
> **Spec references** (X-n / §n) point at `2026-07-05-cache-lifecycle-redesign-design.md`.

> **Double-verification pass (2026-07-08).** Every finding was independently re-derived against the code at `3038a15` by a second set of 6 parallel opus verifiers (freshness core / clear-lifecycle / budget / registry / SWR / P1-remainder + refuted spot-check), each requiring its own quoted `file:line` evidence. **Result: all 16 RB + 13 P1 + 7 P2 findings reproduce.** Net corrections were downward on *severity*, not existence. **One finding was rejected at its stated severity — RB-6 (P0 → PARTIAL); see [Rejected / reclassified on double-verification](#rejected--reclassified-on-double-verification).** Two findings are worse than originally written (P1-8, RB-13). 5 of the 12 "refuted" claims were spot-checked and all were correctly refuted (no real bug buried). Per-finding verdicts are marked **`DV:`** below.

Line numbers are as-of the review; treat them as anchors, not gospel.

---

## Status board

At-a-glance state of every ticket. **✅ done · ➖ decided/covered (no code planned) · ⬜ open.** Detail + evidence in the per-finding sections below; `➖` items are ticked because no work remains, with why noted inline.

**🎉 All 17 RB release-blockers are resolved** — 15 fixed in code, 2 decided (no code). Remaining work is the lower-severity tail only: the P1 list, the P2 list (P2-1 `cacheMaxSizeMB: 0` clamp is the notable one — RB-16 added the registry twin), and the quality-debt items.

**✅ Closed RBs — fixed / covered by code:**
- [x] **[RB-1](#rb-1)** — graph-cache strict content-hash gate (`6eddb2f`)
- [x] **[RB-2](#rb-2)** — read+hash fallback fingerprints for manifest/entry (`e6c98ac`)
- [x] **[RB-3](#rb-3)** — clear-generation guard, cleared entries can't resurrect; adversarially reviewed (`5b4cdfc`)
- [x] **[RB-4](#rb-4)** — force-fresh reads skip the TTL fast path (`83826e2`)
- [x] **[RB-5](#rb-5)** — first-party CJS deps fingerprinted via a cached module set (`204e303`)
- [x] **[RB-7](#rb-7)** — unplugged-drive shard delete fixed inside RB-17's drive-safe `classify_project_root` (`354d297`)
- [x] **[RB-8](#rb-8)** — `remove_shard_by_id` takes the per-shard load lock (cold-open race) (`df1eacb`)
- [x] **[RB-10](#rb-10)** — flush no longer aborts on the first bad entry/shard; aggregates errors, preserves the rest (`df1eacb`)
- [x] **[RB-11](#rb-11)** — eviction progress guard judges bytes freed, not entry_count (`df1eacb`)
- [x] **[RB-12](#rb-12)** — registry `Retry-After` clamped so a 429 can't wedge the pool (`c2dffbe`)
- [x] **[RB-13](#rb-13)** — SWR filters non-cacheable results off the push (daemon + client) (`df1eacb`)
- [x] **[RB-14](#rb-14)** — SWR per-document cancel token + document-scoped revalidation claim (`df1eacb`)
- [x] **[RB-15](#rb-15)** — aggressive zero-threshold compaction when aggregate physical > budget (`df1eacb`)
- [x] **[RB-16](#rb-16)** — `registryCacheMaxSizeMB` wired through Hello end-to-end + zero-guard (`df1eacb`)
- [x] **[RB-17](#rb-17)** — orphan-shard reclaim: drive-safe + automatic + manual button (`354d297` / `6551ce9`)

**➖ Decided RBs — no blocker work planned:**
- [x] **[RB-6](#rb-6-reclassified)** — reclassified P2 (not a blocker); residual closes for free with the freshness core. No independent work.
- [x] **[RB-9](#rb-9)** — insert-path budget enforcement is a deliberate **bounded-overshoot** tradeoff (per-open, ~2×, self-correcting), not a bug (`3500d64`). A synchronous `evict_if_over_budget` is an only-if-needed escalation.

**Non-RB board items:**
- [x] **[P1-12](#p1-12)** — editing deprecated `cacheMaxAgeDays` no longer restarts the daemon (`800823e`)
- [x] **[P1-13](#p1-13)** — debounce-window merge is benign/coherent, replaced ≤300 ms later; no action unless the debounce grows
- [x] **[Cleanups](#cleanups)** — dead `cleanupCache` RPC chain removed (`8f20cfc`); write-only `CachedImport.size_bytes` removed (`53f5b15`); `purge_orphans` **kept** (it's the RB-17 feature, not dead code)
- [ ] **[P1-1 … P1-11](#p1)** — open, below the blocker cut (see the P1 section; each has its own checkbox)
- [ ] **[P2-1 … P2-7](#p2)** — open, plausible/low (P2-1 clamp `cacheMaxSizeMB: 0` is the notable one; drags P2-7)
- [ ] **[Quality debt](#quality-debt)** — duplication / efficiency items open (the two dead-code items above are done)

---

## Decisions

Design/scope decisions made while working this backlog now live in a dedicated
log: **[`docs/cache-lifecycle-decision-log.md`](../../cache-lifecycle-decision-log.md)**
(cleanups D1, orphan-reclaim-as-feature D2, maintenance scheduling D3, standing
policies, and parked items). Per-finding status notes (`DV:` / commit refs) stay
inline below.

---

## P0 — Release blockers (confirmed)

<a id="rb-1"></a>

### RB-1. Graph cache uses the non-strict probe → permanent first-party stale loop
- **Where:** `daemon/src/pipeline/graph.rs:398` (also `:463`), via `fingerprints_are_current` (`key.rs:318`, non-strict)
- **Mechanism:** The module-graph cache gates reuse with the mtime+len pre-filter even for first-party modules, while L2 uses `check_fingerprints_strict`. An equal-length, mtime-preserving rewrite (cp -p, rsync -a, tar -x) makes L2 evict + recompute, but the recompute pulls the SAME stale graph from the graph cache and re-inserts the stale result with old hashes.
- **Impact:** Stale sizes served forever **plus** a full bundle+minify+compress re-analysis on every poll. Nothing on the recompute path invalidates a first-party graph entry (`graph.rs:536` invalidation only matches `node_modules/<pkg>/`).
- **Fix shape:** strict (content-hash) gate for first-party fingerprints in the graph cache, or invalidate the graph entry whenever L2's strict probe returns Stale.
- **DV: ✅ CONFIRMED.** Sharpest of the set — re-opens the exact X-7 blind spot that the strict L2 check was built to close, because the graph cache (the recompute source) was never migrated to strict. Correctness bug *and* a full re-analysis cost on every poll.
- **✅ FIXED 2026-07-08 (`6eddb2f`).** Both graph-cache reuse gates (build + peek) now use `check_fingerprints_strict` (hash-verifies first-party modules, keeps the cheap pre-filter for node_modules); reuse on transient `Unknown`, rebuild on `Stale`/`Gone`. Regression test: an equal-length, mtime-preserved first-party edit rebuilds the graph.

<a id="rb-2"></a>

### RB-2. Post-analysis, hashless fingerprints re-open the X-1 TOCTOU
- **Where:** `daemon/src/service.rs:2090-2103` (no-graph fallback: `[package.json, entry]`, hash `None`, stat'd AFTER analysis at `:1884`); `daemon/src/pipeline/graph.rs:521-523` (manifest on the graph path, hash `None`)
- **Mechanism:** §4.2 requires capture-before-read with content hashes. These paths stat after analysis with no hash; `check_fingerprint_strict` falls back to the cheap pre-filter when `content_hash` is `None` (`key.rs:285`).
- **Impact:** A file modified during the analysis window stores the NEW mtime+len → result computed from OLD bytes probes Fresh indefinitely. Covers the manifest on *every* graph analysis and everything on the no-graph path.
- **Fix shape:** hash the manifest/entry bytes at read time on all paths (the hash *is* the read, per §4.2).
- **DV: ✅ CONFIRMED.** Scope refinement: "probes Fresh forever" holds only where no generation bump ever lands (first-party entry, or a watcher-missed node_modules change) — a normal install bump re-verifies. It is a bounded-window capture-after-read TOCTOU, not unconditional permanent-stale.
- **✅ FIXED 2026-07-08 (`e6c98ac`).** New `file_fingerprint_reading_hash` read+hashes the manifest and the no-graph entry (the hash IS the read, §4.2); wired into `dependency_fingerprints`' no-graph branch and `fingerprints_with_content_hashes`' non-module fallback. A first-party same-length edit is now caught; node_modules keeps the generation backstop. **Residual:** a narrow *same-request* TOCTOU (a file changing mid-analysis) — full capture-before-read would need read-time hash threading (overlaps the RB-5 CJS work).

<a id="rb-3"></a>

### RB-3. `clear()` races in-flight writers — cleared entries resurrect (4 unguarded paths)
- **Where:** anchor `daemon/src/cache/memory.rs:924` (flush_to_disk); also `disk.rs:820-846`, `memory.rs:675-688`, `memory.rs:732-738`
- **Mechanism:** No clear-epoch/guard between `clear()`/`remove` and concurrent writers:
  1. `flush_to_disk` dirty replay + promoted-recency sweep snapshot memory, then `disk.insert` per entry with no lock/epoch — a clear between snapshot and inserts is undone.
  2. `DiskCache::clear()` commits the table wipe (`disk.rs:841`) BEFORE `pending_inserts.clear()` (`:843-845`); a concurrent `flush_pending_inserts` drains and commits pre-clear entries into the wiped shard.
  3. Insert is disk-then-memory (`memory.rs:675` → `:688`): a clear in between leaves a memory-only entry that is not dirty and has `persisted_seq == last_seq` — invisible to flush and evictor, survives Clear until process exit (and a later hit re-persists it via the sweep).
  4. `enforce_memory_cap` re-persists a promoted victim via `disk.insert` (`:732-738`) with no epoch check.
- **Impact:** "Clear cache" silently doesn't clear — the exact trust/honesty failure the redesign was built to end. Same race class commit `14eb293` fixed for the registry.
- **Fix shape:** a clear-epoch counter checked by all writer paths (flush, insert completion, cap re-persist), or one lock spanning wipe+pending-clear and writer commits.
- **DV: ✅ CONFIRMED.** All four paths reproduce; the `disk.rs:841` commit-before-`pending.clear` ordering and the no-epoch insert/flush windows are exactly as described.
- **✅ FIXED (`5b4cdfc`).** `DiskCache` gained a `clear_generation` (AtomicU64) + `clear_lock`. Pending inserts are tagged with the generation captured when their bytes were derived; `clear()` holds `clear_lock`, bumps the generation first, then wipes + drops pending; `flush_pending_inserts` holds `clear_lock` and writes only current-generation entries; `pending_insert_entry` won't serve a superseded one. `clear()` and `flush_pending_inserts` each hold `clear_lock` across their whole redb transaction, so they can't interleave. `flush_to_disk`/`enforce_memory_cap` capture the generation before their snapshot and use `insert_at_generation`; the fresh insert + both disk-hydration reads route the memory insert through `insert_into_memory_guarded` (pre-check skip + identity-checked rollback). Bumped even when disk is disabled → memory-only mode covered. Tests: disk-layer stale-generation-dropped-after-clear + memory rollback guard. **Adversarially reviewed (opus):** no remaining resurrection window, no deadlock; one self-inflicted miss-only regression found and fixed (the identity check). See decision-log D7 for the accepted residual.

<a id="rb-4"></a>

### RB-4. `get_if_fresh` (CI force-fresh) rides the TTL fast path unverified
- **Where:** `daemon/src/cache/memory.rs:249-253` (fast path ignores `intent`), served at `:316-319`; contract at `:209-215`; caller `service.rs:1862`
- **Mechanism:** `fresh_without_restat = !first_party && generation matches && verified_at < REVERIFY_TTL(30s)` — computed without consulting `ReadIntent::RequireFresh`.
- **Impact:** A node_modules change with no generation bump (watcher-excluded folder — the case `memory.rs:31-33` says the TTL exists for) is served with zero re-verification inside the 30s window; the `importlens check` budget gate judges against a stale size, violating §4.5 "CI/CLI forces fresh".
- **Fix shape:** `RequireFresh` must always run the probe (skip the TTL gate).
- **DV: ✅ CONFIRMED.** Mechanism exact: the disk path honors `RequireFresh` (`memory.rs:346-352`) but the memory fast path did not. The `memory.rs:30-33` comment justifies the SWR fast path for *normal* reads only; extending it to force-fresh is the undocumented defect.
- **✅ FIXED (`83826e2`).** `fresh_without_restat` now leads with `!matches!(intent, ReadIntent::RequireFresh)`, so a force-fresh read always falls through to the tri-state re-probe. Regression test `force_fresh_read_never_rides_the_ttl_fast_path` (normal `get` serves via the fast path; `get_if_fresh` re-probes, detects the stale dep, evicts, returns `None`).

<a id="rb-5"></a>

### RB-5. First-party CJS deps: fingerprints degrade to manifest+entry — edits never invalidate
- **Where:** `daemon/src/pipeline/analyze.rs:170-172` (`analyze_with_cjs_graph` returns `(result, None)`); fallback set `service.rs:2090-2097`; L1 hole `file_size_cache.rs:214-215`
- **Mechanism:** No graph is returned for CJS, so transitively `require()`'d modules are not fingerprinted; L1's `first_party_module_token` also degrades to the entry stat (GRAPH_CACHE never populated by the CJS analyzer).
- **Impact:** Editing `lib/impl.js` of a workspace/`file:`/npm-link CJS dep never invalidates — permanent stale sizes until entry/manifest change (the D3 class).
- **Fix shape:** return and fingerprint the CJS require-graph (it is already walked), or force-probe/short-TTL first-party CJS entries.
- **DV: ✅ CONFIRMED.** Both the L2 hole (entry+manifest only) and the L1 hole (`stat_token(entry)` alone because GRAPH_CACHE is empty for CJS) reproduce. node_modules CJS is saved by the generation bump; first-party CJS is the exposed class.
- **✅ FIXED (`204e303`).** The CJS walk now caches its module set (canonical paths + read-time len/mtime/content-hash fingerprints) keyed by the canonical entry, in a bounded LRU `CJS_MODULE_CACHE` mirroring `GRAPH_CACHE`. L2 `dependency_fingerprints` folds the fingerprints into the no-graph branch (gated on `resolved.is_cjs`, deduped); L1 `first_party_module_token` peeks the paths via `peek_cjs_module_paths` when no ESM graph is cached. Cache cleared/invalidated/purged on the same seams as the graph cache (cache-remove `All`/shard-removing, `invalidate_package`/`invalidate_all`, package-change bursts, orphan sweep). Tests: L1 transitive path coverage, L2 content-hash catch of a mtime-preserving equal-length deep edit, clear + purge seams.

<a id="rb-6"></a>

### RB-6. Generation snapshot taken AFTER resolution (prefetch window is huge)
> **⚠️ Rejected at P0 on double-verification — reclassified to P2 (PARTIAL).** See [Rejected / reclassified on double-verification](#rejected--reclassified-on-double-verification). The structural claim (capture-after-resolve, no per-job re-resolve) is true, but the review missed two mitigations, so the "huge window / entries probe Fresh" impact does not hold as a blocker.

<a id="rb-7"></a>

### RB-7. Unplugged Windows drive → `NotFound` → valid shard permanently deleted (X-3)
- **Where:** `daemon/src/cache/key.rs:147-152` (`path_is_definitely_gone`), `:218-219` (`classify_stat_error`); destructive caller `project.rs:586-590` → `remove_shard_by_id`
- **Mechanism:** Windows returns `ERROR_PATH_NOT_FOUND` for a released drive letter; Rust maps it to `ErrorKind::NotFound`, which the code treats as definitively gone. The comment at `project.rs:571-573` ("offline drive is kept") only holds for `ERROR_NOT_READY`-style errors.
- **Impact:** Running the orphan purge with a project drive unplugged destroys the shard; entry-level probes likewise report Gone.
- **Fix shape:** before destructive reclaim, verify the path's root (drive/volume) still exists; treat missing-root as Unknown → keep.
- **DV: ✅ CONFIRMED — blast radius narrower than framed.** The destructive shard-delete is reachable only via the manual "Purge Orphans" trigger (`purge_orphans`), not the automatic maintenance path.
- **⚠️ DECISION (2026-07-08): do NOT retire `purge_orphans`.** Orphan reclaim is a wanted feature (see [RB-17](#rb-17-orphaned-project-cache-shards-are-never-proactively-reclaimed--purge-must-be-drive-safe)); the drive-safety fix here is a *prerequisite* of RB-17.
- **✅ CLOSED 2026-07-08 (`354d297`)** as part of RB-17: `classify_project_root` distinguishes a genuinely-deleted root (`Orphaned`) from an unreachable volume (`VolumeUnreachable` → keep), so an unplugged drive no longer destroys a valid shard. Unit-tested via an injected existence probe.

<a id="rb-8"></a>

### RB-8. `remove_shard_by_id` skips the load lock — races cold open; Windows half-removed state
- **Where:** `daemon/src/cache/project.rs:704-709` (no `load_lock_for`), `:754` (`remove_dir_all`), error path `:769-775`; cold open holds the lock `:322-366` but registers only at `:364-366`
- **Mechanism:** Remove between a cold open's DB-open and registration finds nothing in `loaded` (skips `cache.clear()`), deletes the dir under a live redb handle, then the open registers — a live shard over a deleted directory, resurrected by the next `write_metadata` (`:994`). On Windows, `remove_dir_all` failure surfaces "removal failed" for a shard already unregistered + cleared.
- **Fix shape:** take the per-shard load lock in `remove_shard_by_id` (same protocol as `cache_for_root`).
- **DV: ✅ CONFIRMED.** The register-after-open gap and the missing load lock in the remove path reproduce.
- **✅ FIXED (`df1eacb`).** `remove_shard_by_id` now takes the per-shard load lock (`load_lock_for`) before touching `loaded`, the same `load_lock → loaded` order as `cache_for_root`, so a cold open can't register a shard while its dir is being deleted. Test: removal blocks behind a held load lock and completes on release. (Reviewer note: a manual purge racing a maintenance *temp-open* of an unloaded shard is a pre-existing, out-of-scope residual.)

<a id="rb-9"></a>

### RB-9. Budget eviction runs ONLY on the 60s tick — no insert-path enforcement (§5.3 unimplemented)
- **Where:** `daemon/src/cache/project.rs:178` (`evict_to_budget` — sole production caller `service.rs:1605` via the tick at `server.rs:38/96-101`)
- **Mechanism:** The design (and plan doc `2026-07-06-cache-capacity-budget.md:114`) requires eviction after each insert-flush. The old unconditional per-shard caps (512 MB / 20k entries) were deleted; nothing replaced their synchronous bound.
- **Impact:** A cold workspace-report/prefetch burst overshoots `cacheMaxSizeMB` arbitrarily for up to 60s+ — on nearly-full disks that is user-visible harm.
- **Fix shape:** call `evict_if_over_budget` from the flush path (cheap check against the rollup, evict only when over high-water).
- **DV: ✅ CONFIRMED — load-bearing.** `evict_to_budget` has no production caller other than the maintenance pass, and that single enforcement path is *itself* defeated by RB-11 (false "exhausted" retirement), RB-15 (physical/logical gate mismatch), and nulled by P2-1 (`budget==0`). These four must be treated as one unit — piecemeal fixes won't restore capacity enforcement.
- **⚠️ SCHEDULING UPDATE (2026-07-08, `3500d64`):** enforcement now runs **once per project-open** (not every 60 s). This is a deliberate design choice: a single project's cache converges to its import footprint, so bounded overshoot (~2× budget worst case, self-correcting on next open/relaunch) is accepted instead of adding synchronous insert-path eviction. RB-11 / RB-15 / P2-1 still matter for correctness *within* a pass, but the "60 s overshoot window" framing is superseded — the window is now "until next project-open." A synchronous `evict_if_over_budget` on the flush path remains the clean fix if the bounded overshoot ever proves too loose in practice.

<a id="rb-10"></a>

### RB-10. Shutdown/recycle flush aborts on first failing dirty entry — and skips all remaining shards
- **Where:** `daemon/src/cache/memory.rs:932-938` (early `return Err` before the promotion sweep and `flush_pending_inserts`); `project.rs:697-698` (`cache.flush_to_disk()?` aborts the all-shards loop); shutdown caller `server.rs:858`
- **Mechanism:** One deterministically-failing dirty entry (serialization error) poisons every flush. Bonus defect: the error path re-marks ALL taken `dirty_keys` dirty (`:935`), including ones already successfully re-inserted this pass.
- **Impact:** On shutdown: queued-but-uncommitted inserts lost, session recency promotions never persisted (cross-restart LRU fidelity lost), and every other shard's flush skipped. Repeats on every idle-recycle until restart.
- **Fix shape:** per-entry error collection (never abort the loop), always run the sweep + pending flush, aggregate errors across shards.
- **DV: ✅ CONFIRMED.** The early `return Err` before the sweep + pending-flush, and the all-shards loop abort, reproduce.
- **✅ FIXED (`df1eacb`).** `flush_to_disk` (memory + project) collects per-entry/per-shard errors instead of aborting, excludes failed keys from the recency sweep (they share the `persisted_seq` Arc), re-marks ONLY the failed keys dirty (fixes the re-mark-all bonus defect), always runs the pending flush, and recovers a poisoned `loaded` lock. Tests: dirty-replay-after-one-fails + registry-flush-attempts-every-shard.

<a id="rb-11"></a>

### RB-11. Eviction progress guard uses entry_count, not bytes freed — active shard escapes the budget
- **Where:** `daemon/src/cache/budget.rs:143-163` (`after.entry_count >= before_count` → exhausted; `freed` never consulted; `victim.rollup()` flushes pending inserts via `disk.rs:448`, refilling the count)
- **Impact:** While insert rate ≥ ~128 entries/tick, the one over-budget shard is retired as "exhausted" every pass and the budget is unenforced exactly when it matters.
- **Fix shape:** judge progress by `freed > 0` (bytes), not entry_count.
- **DV: ✅ CONFIRMED — timing refined.** The guard checks `entry_count`, not `freed` bytes; a concurrent flush refills the count and trips the false "exhausted" retirement. Correction: this is transient/self-healing per eviction round (High-during-burst), not a permanent leak — but during a sustained burst it defeats the only enforcement path (RB-9).
- **✅ FIXED (`df1eacb`).** The progress guard is now `freed == 0` (bytes actually reclaimed), not `entry_count`, so a concurrent flush refilling the count can no longer falsely retire an over-budget shard mid-burst. Test: a byte-freeing shard with a replenishing entry_count keeps evicting past the first batch.

<a id="rb-12"></a>

### RB-12. Uncapped `Retry-After` wedges the registry pool (with single-flight waiters behind it)
- **Where:** `daemon/src/registry/service.rs:118-123` (`apply_retry_after`, no clamp), `:607-608` (`thread::sleep(wait)` on the 4-thread pool), `client.rs:75` (raw parse); sleeper owns the single-flight slot (`:383`, waiters `:394-400`)
- **Impact:** A 429 with `Retry-After: 3600` (proxy-controlled) hangs all registry hints and every waiter for an hour, uncancellable.
- **Fix shape:** clamp Retry-After (e.g. ≤ 5 min), and prefer failing fast + surfacing "backed off until T" over sleeping pool threads.
- **DV: ✅ CONFIRMED — single highest-urgency item.** Timing correction: the 429 response that *installs* the backoff returns immediately (`service.rs:502`); it does not itself sleep. The wedge lands on the *next* reservations — up to 4 pool workers `thread::sleep` for the full unclamped Retry-After, each owning a single-flight slot, with same-package condvar waiters blocked behind them. Uncancellable (no timeout/cancel token). Fix surface is small: clamp in `client.rs` / `apply_retry_after`, and/or sleep in cancellable slices.
- **✅ FIXED 2026-07-08 (`c2dffbe`).** `apply_retry_after` clamps the global backoff floor at `REGISTRY_MAX_BACKOFF_MS` (5 min), so an unbounded upstream `Retry-After` can no longer park the pool. Regression test: an hour-long Retry-After yields a ≤5 min reservation wait. (Chose the clamp over fail-fast/cancellable-sleep — the clamp alone removes the unbounded-wedge harm; the 5 min bound is an acceptable honoring of a genuine rate-limit.)

<a id="rb-13"></a>

### RB-13. SWR pushes an error result over the good stale value
- **Where:** `daemon/src/service.rs:955` (`fresh.push(result)` unconditional; `should_cache_result` at `:1883/:1991` rejects the same result for caching); client merges without an error check (`refreshMerge.ts:94`)
- **Impact:** A transiently unreadable dep after a Stale probe → UI replaces a valid last-known size with an error state while the cache still serves the stale value — display and cache disagree; breaks the serve-last-known promise exactly when SWR matters.
- **Fix shape:** filter error results from the push (or push them only when the cache also accepts them).
- **DV: ✅ CONFIRMED — worse than framed.** The review pins the trigger on "a transiently unreadable dep," but that case is already defended (`service.rs:918-923` re-probes and `continue`s on Unknown). The *more* reachable trigger is a genuinely content-`Stale` dependency whose recompute yields **any** error (parse/bundle failure on the changed content, resolution regression, or a TOCTOU read failure). Every such error is pushed unfiltered and stamped `status:"ready"` on the client while the cache keeps the old good value — and because the cache still serves `Stale`, the next `file_size_document` re-triggers it: a repeating error-push loop on each size read.
- **✅ FIXED (`df1eacb`).** Daemon `revalidate_document_sizes` filters non-cacheable results (`should_cache_result`) off the push and returns `None` when nothing remains; the client `refreshMerge` drops errored results. See **decision-log D8** for the push⟺cacheable rationale + the same-specifier-test reconciliation. Tests: `omits_non_cacheable_results` (daemon) + "ignores errored refreshed results" (client).

<a id="rb-14"></a>

### RB-14. SWR revalidation starved: prefetcher's global cancel token + per-key dedupe drops pushes
- **Where:** `daemon/src/ipc/server.rs:797-798` (borrows `prefetcher.cancellation()`; nearly every message calls `prefetcher.cancel()` — `server.rs:372/423/449/492/514/518/533/556/684/720/742/764/832/854`); `service.rs:927` (`begin_revalidation` per cache key — second document sharing the key gets no push, no re-arm)
- **Impact:** Under exactly the active-editing load SWR was designed for, "revalidating" badges resolve late or never until the document's next request. Self-heals on next poll, but defeats the feature's purpose.
- **Fix shape:** give SWR its own supersession scope (e.g. keyed by document/analysis generation), and either push to all coalesced documents or re-arm losers.
- **DV: ✅ CONFIRMED — severity bounded to eventual consistency.** The push does eventually land once the user pauses (the final analyze cycle's `file_size_document` spawns a revalidation nothing later cancels). The genuine defect is the *global shared token*: an unrelated document's `analyze_document`, `Shutdown`, `WorkspaceReport`, `CompleteImportMembers`, or a prewarm all bump the same generation and cancel this document's in-flight revalidation; the per-key dedupe compounds it for a second document on the same key. "Late, and cancellable by unrelated activity," not "never."
- **✅ FIXED (`df1eacb`).** SWR gets its own per-document cancel token (`SwrRefreshLifecycle`, keyed by workspace+document) replacing the prefetcher's global token, plus a document+generation-scoped revalidation claim (not the bare cache key). See **decision-log D9** (accepted cross-document redundant recompute; bounded per-connection map). Tests: per-document cancel isolation + a raw-key claim no longer starves a document's push.

<a id="rb-15"></a>

### RB-15. Maintenance gate compares PHYSICAL bytes; evictor uses LOGICAL; compaction needs >50%/shard — reachable do-nothing spin
- **Where:** `daemon/src/cache/project.rs:207` (physical gate via `total_shard_file_bytes`, `:287-299`) vs `budget.rs:106-108` (logical) vs `disk.rs:57/723` (`COMPACT_THRESHOLD=0.5` per shard)
- **Impact:** Physical > budget, logical ≤ budget, fragmentation spread ≤50%/shard → every 60s pass temp-opens all shards (create+heal txns; slow under Windows AV), evicts nothing, compacts nothing, forever; physical footprint never tracks the budget (the G-1 gap §5.5 claims to close).
- **Fix shape:** compact on aggregate physical-over-logical overhang (not only per-shard ratio), or gate the pass on logical bytes and drive compaction from the physical/logical delta.
- **DV: ✅ CONFIRMED — conditional.** The do-nothing spin requires fragmentation spread thinly across many shards (each ≤50%, aggregate over budget). But even short of that, the pass pays a full temp-open of every shard each tick to accomplish nothing. Physical/logical/per-shard-ratio triple-mismatch is real.
- **✅ FIXED (`df1eacb`).** After the normal per-shard compaction, if aggregate physical bytes still exceed the budget an aggressive zero-threshold pass compacts every idle shard with any reclaimable space, so thinly-spread free pages are reclaimed and the physical footprint tracks the budget. Test: the aggressive pass runs only when physical > budget, at threshold 0.0. (Frequency was already defanged by `3500d64` — per-open, not per-minute.)
- **Scheduling note (`3500d64`):** the pass now fires once per project-open, not every 60 s, so the wasted temp-open happens at most once per open instead of per minute. The gate mismatch itself is unchanged and still worth fixing.

<a id="rb-16"></a>

### RB-16. `importLens.registryCacheMaxSizeMB` is a declared no-op
- **Where:** `package.json:178-182` (declared, default 32, description promises enforcement); `config.ts` never reads it; Hello never carries it (`nativeTransport.ts:717-718`); daemon hardcodes `REGISTRY_CACHE_MAX_SIZE_BYTES = 32 MiB` (`registry/constants.rs:39-42` admits the wiring is a follow-up)
- **Fix shape:** wire it through Hello, or pull the setting from the manifest until it does something.
- **DV: ✅ CONFIRMED.** The setting is declared/defaulted/documented-as-enforced but never read on the extension side nor transmitted; daemon always applies the 32 MiB constant. Note: at the *default* value it coincidentally equals the constant, so the no-op only bites a non-default (hand-edited) value.
- **✅ FIXED (`df1eacb`).** Wired end-to-end through Hello: extension `config.ts`/`protocol.ts`/`nativeTransport.ts`/`configChange.ts` → `HelloMessage.registry_cache_max_size_mb` (serde-defaulted for old clients) → `ImportLensService.registry_cache_max_size_bytes` → the maintenance size cap (replacing the hardcoded constant). A `0` value is guarded to mean "no cap" (not "evict everything") — see **decision-log D10**; a proper minimum-clamp is the shared P2-1 follow-up. Tests: Hello decode (explicit + omitted-default), daemon storage, registry zero-guard.

<a id="rb-17"></a>

### RB-17. Orphaned project-cache shards are never proactively reclaimed (+ purge must be drive-safe)
> **Source:** product-owner review, 2026-07-08 (not from the adversarial sweep). Supersedes the earlier "retire `purge_orphans`" direction — orphan reclaim is a wanted feature; it must be made **safe** and **automatic**, not deleted.

- **Scenario (owner's words):** "I had a project which had a lot of caches. Now I don't have that project — maybe moved or removed, or maybe some of the files are removed — but they still occupy space. It's even more of a problem if that project was recently opened."
- **The gap:** "Orphan" spans two cases. **(a) Stale entries inside a project you still open** (uninstalled/updated package) — already handled automatically (targeted name-invalidation on `NodeModulesChanged` + the freshness probe's `Gone` eviction on access + analyzer-version staleness riding the byte budget). **(b) A whole abandoned project's shard** (project moved/deleted, never reopened) — **NOT handled proactively.** Nothing scans on-disk shards for a project root that no longer exists; the automatic reclaim only fires *on access* or *on a node_modules event*, and an abandoned project triggers neither. Those bytes fall only to the global LRU byte budget (`BudgetCoordinator`), which evicts **least-recently-used first** — so a *recently* opened-then-abandoned project (high `last_seq`) is evicted **last** and lingers the longest. Exactly the owner's worst case.
- **What exists today:** `ProjectCacheRegistry::purge_orphans` (`daemon/src/cache/project.rs:523`) already does the right shape — for each shard, if the project root is gone, `remove_shard_by_id`; for surviving shards, `purge_orphan_entries`. But it is (1) **manual-only and unreachable** — the Manage-Cache UI item was retired by the redesign (extension asserts the `purgeOrphans` action is absent — `extension/test/ui/cacheManagerItems.test.ts`), leaving the daemon `CacheRemoveScope::Orphans` scope with no trigger; and (2) **drive-unsafe** — see RB-7.
- **Requirements (owner-approved 2026-07-08):**
  1. **Drive-safe shard scan (fixes [RB-7](#rb-7-unplugged-windows-drive--notfound--valid-shard-permanently-deleted-x-3)).** Removing a shard for a "gone" root must distinguish a *genuinely deleted* project folder (its parent/volume is reachable, the folder is absent → orphan → remove) from an *unreachable volume* (unplugged/offline drive → keep). Root cause: `path_is_definitely_gone` (`daemon/src/cache/key.rs:8`) treats `ErrorKind::NotFound` as gone, but Windows maps a released drive letter's `ERROR_PATH_NOT_FOUND` to `NotFound` — so an offline drive currently reads as "deleted." Fix shape: before concluding a root is deleted, confirm the volume/root prefix (e.g. `D:\` / the UNC share) is itself reachable; treat an unreachable volume as *keep*. (`ERROR_NOT_READY` already maps away from `NotFound`; the specific trap is `ERROR_PATH_NOT_FOUND` on a removed drive letter.)
  2. **Automatic sweep on the maintenance tick** (`ImportLensService::run_cache_maintenance`, `daemon/src/service.rs:1538`, driven by the 60 s `CACHE_MAINTENANCE_INTERVAL` task in `server.rs:98-103` via `spawn_blocking` — already OFF the connection loop, so non-blocking). Owner: run it here rather than at startup ("we won't face this so often"). **Throttle it** — do not scan every 60 s tick; gate to e.g. once per hour or once per daemon session (a `last_orphan_sweep` timestamp guard), since abandoned-project detection is rare and the scan temp-opens shards. **Explicitly NOT at startup** — startup must stay non-blocking (Hello handshake latency, cf. P1-8); only add a startup pass later if it can be made fully async/off-handshake.
  3. **Keep the manual button.** Re-add the Manage-Cache "remove orphan caches" action (extension UI) wired to the existing `CacheRemoveScope::Orphans` daemon scope, so the user can trigger the same safe sweep on demand.
  4. **Un-deprecate the framing.** Drop the "DEPRECATED (§7/§8) / retire in Part F / back-compat only" doc comments on `purge_orphans` (`project.rs:508-517`), the `Orphans` variant (`protocol.rs`), and the `Orphans` branch (`service.rs:1457-1464`) — it is now a supported, first-class feature.
- **Explicitly out of scope of the "no deprecated/back-compat code" cleanup:** because of this decision, `purge_orphans` / `purge_orphan_entries` / `cache_key_is_orphan` / `purge_missing_paths` / `purge_missing_module_graphs` / the `Orphans` scope are **retained** (they are the feature, not dead code). The back-compat sweep must skip them.
- **Verification to add:** a test that a shard whose root is genuinely deleted is removed by the sweep, AND a test that a shard whose *volume is unreachable* (simulated `ERROR_PATH_NOT_FOUND` / a mocked stat error distinct from a real absent path) is **kept** — the RB-7 regression guard. Plus: the automatic tick actually invokes the sweep (throttle honored), and the recently-abandoned shard is reclaimed without waiting for the byte budget.
- **Status:** ✅ IMPLEMENTED 2026-07-08 — `354d297` (daemon: `classify_project_root` drive-safe classifier closing RB-7, `purge_orphans` refactor, throttled `sweep_orphaned_shards_if_due` on the maintenance tick, unit + integration tests) and `6551ce9` (extension: Manage-Cache "Remove Orphaned Caches" action). Verification from the requirements is covered: unit tests assert the unplugged-drive path is kept (`VolumeUnreachable`) and a genuinely-deleted root is `Orphaned`; an integration test asserts the tick sweep reclaims an abandoned shard, keeps live shards, and throttles.

---

<a id="p1"></a>

## P1 — Confirmed, below the blocker cut

- [ ] **P1-1. Select-then-evict hot-shield race** — `memory.rs:884-907`: `filter_evictable` snapshots hotness, `evict_keys` deletes disk row + memory mirror with no re-check; an interactive `get` promoting in between still loses the entry. Bounded (recompute on next access) but provably violates the hot-shield guarantee.
  - **DV: ✅ CONFIRMED** (moderate — genuine TOCTOU, recompute not data loss).
- [ ] **P1-2. Insert-side recency flooding (scan resistance is read-only)** — `memory.rs:661-670`: every insert stamps a fresh `born_seq`; a bulk report/prefetch that MISSES widely inserts thousands of newer-seq entries, making the user's warm set the LRU victim under disk-budget pressure (§5.1 goal partially defeated).
  - **DV: ✅ CONFIRMED.** Scan resistance is read-only by construction (the insert path has no `intent`/bulk parameter, unlike reads which gate via `ReadIntent`); bulk-MISS inserts sort above the un-re-promoted warm set and evict it.
- [ ] **P1-3. Interactive analyze paths never promote recency** — `service.rs:1862` force-fresh branch (`get_if_fresh`, non-promoting) serves Batch/AnalyzeDocument/FileSize/AnalyzeSpecifiers/analyze_package_json; the "CI-only" comment at `:1854` is wrong. Mostly masked because the extension follows with `FileSizeDocument` (promoting) for the same keys; residue: package.json dependency view and Batch-only consumers.
  - **DV: ✅ CONFIRMED.** The interactive `intent` argument is ignored in the else-branch (only used in the `serve_stale` branch); the "CI-only" comment is inaccurate. Masked, not eliminated.
- [ ] **P1-4. `cache_full_variant_alias` uses a promoting, fully-verifying `get` as an existence check** — `service.rs:1929`: runs on every recompute (incl. bulk paths) → scan-resistance leak + a wasted strict verification; `get_for_prewarm` (non-promoting) exists and isn't used.
  - **DV: ✅ CONFIRMED.** `get` promotes `last_seq` and runs the full strict fingerprint verify; called on every recompute including the `ReadIntent::Bulk` WorkspaceReport path. Should use `get_for_prewarm`.
- [ ] **P1-5. Superseded bulk registry refreshes surface fabricated per-target errors** — `server.rs:648-655` fills cancelled slots with `"worker did not return a result"`; daemon supersession is per-connection while the client guard (`registryRefresh.ts:128-136`) is per (uri, target) → a refresh for a different package.json surfaces stale/error hint state for intentionally superseded work.
  - **DV: ✅ CONFIRMED.** Strongest form is cross-document: if block B targets a different uri than block A, the client bumps generations only for B's keys, so every unfinished target of A is non-superseded and surfaces the fabricated error. Low-moderate; self-heals on next refresh.
- [ ] **P1-6. No document-level freshness marker (spec §6.2)** — `protocol.rs:334-346` (`FileSizeDocumentResponse`) carries no freshness; `currentFileSize.ts` has no staleness handling; the status-bar total can advertise itself while components are revalidating; the promised "future UI is pure presentation" doesn't hold.
  - **DV: ✅ CONFIRMED (substance) — one term wrong.** Per-import `ImportResult` *does* carry `freshness` (`protocol.rs:209`) but the aggregate totals do not, and `currentFileSize.ts` gates only on `error`. There is **no `CachedFileSize` struct** — the totals aren't cached at all (recomputed per request; disk stores per-import freshness normalized to `Fresh`). The document-level-marker gap is real; drop the `CachedFileSize` phrasing.
- [ ] **P1-7. Compaction idle gate uses wall clock (X-6 class)** — `disk.rs:703-705` + `time.rs:11-13` (`SystemTime`); forward NTP jump/resume → exclusive-lock `database.compact()` (`:728-745`) mid-analysis (COMPACT_IDLE is only 5s); backward jump disables compaction.
  - **DV: ✅ CONFIRMED.** Forward jump inflates `idle_for` → passes the 5s idle gate on an actively-analyzed shard → exclusive-lock compact stalls concurrent gets; backward jump → `saturating_sub`→0 → compaction disabled. Should be `Instant`.
- [ ] **P1-8. Hello handshake sequentially opens every lifetime shard** — `server.rs:350` → `project.rs:161-171` (`seed_recency_clock_from_disk`): full DB open + heal txn per shard to read one SUMMARY scalar; startup latency linear in lifetime project count (bad under Windows AV). Fix: persist `max_seq` in the JSON sidecar or defer off-handshake.
  - **DV: ✅ CONFIRMED — understated.** `ensure_schema` runs a **write** transaction (`begin_write`+`commit`) per shard, plus `heal_summary_if_inconsistent`, not just a scalar read. Handshake blocks on this sequentially; worse than the original wording.
- [ ] **P1-9. Manual + background registry budgets share one rate window** — `registry/service.rs:141-161`: a manual cap-hit advances `window_opens_at` and resets `request_count`, stalling background fetches that had budget; one ForceRefresh of a >5-dep manifest triggers it. Severity bounded (1s window).
  - **DV: ✅ CONFIRMED — downgrade to low.** Mechanism confirmed, but the stall ceiling is one `REGISTRY_RATE_LIMIT_WINDOW_MS` = 1s, so it's a ≤1s throttle/fairness wart, not a stall of meaningful duration.
- [ ] **P1-10. Registry snapshot rewritten authoritatively every 60s even when nothing changed** — `registry/cache.rs:225-258` (no removed==0 / dirty gate; `unpersisted_writes` reset but never checked) via `service.rs:1626`: full load+parse+serialize+rename per minute per VS Code window; amplifies the documented read→rename clobber (`cache.rs:310-312`). **Largely defanged by `3500d64`:** maintenance now runs once per project-open, so this no longer fires per-minute; the missing dirty-gate is still a cheap correctness/IO nicety but is no longer a per-minute cost.
  - **DV: ✅ CONFIRMED.** No dirty/no-op gate; `write_snapshot` at `cache.rs:258` runs unconditionally and the reset `unpersisted_writes` counter is never consulted for the decision. Per-window IO waste that also widens the clobber race. Medium.
- [ ] **P1-11. Registry eviction boundary-key iterator bug** — `registry/cache.rs:455-469`: phase-1 `for` consumes the boundary key before breaking; phase 2 resumes after it → the oldest remaining entry escapes and newer entries evict instead (order violation only; budget still enforced).
  - **DV: ✅ CONFIRMED** (low). The `by_ref()` boundary consumption is a real Rust iterator gotcha; order-only, budget still enforced. Only manifests when the phase-1 estimate leaves the snapshot fractionally over budget.
<a id="p1-12"></a>

- [x] **P1-12. Editing deprecated `cacheMaxAgeDays` restarts the daemon** *(✅ fixed `800823e`)* — `configChange.ts:16-18` still classifies it `daemonRestart` (and `configChange.test.ts:12` locks it in) though `package.json:175-176` documents it as ignored; full daemon bounce for a no-op setting.
  - **DV: ✅ CONFIRMED.** Editing a documented-ignored no-op setting triggers a full daemon restart (tears down the in-memory cache).
<a id="p1-13"></a>

- [x] **P1-13. Debounce-window merge (benign, note only)** *(➖ decided: benign — no action)* — `listener.ts:100-108`: `freshness.begin` fires only in `analyze()`, so a push landing inside the debounce window merges onto pre-edit states; coherent with what's on screen and replaced ≤300ms later. No action needed unless the debounce grows.
  - **DV: ✅ CONFIRMED benign.** Coherent, self-consistent value replaced ≤`debounceMs` later; only bites if it coincides with the RB-13 error case.

---

<a id="p2"></a>

## P2 — Plausible (mechanism real; trigger uncertain or requires unusual config)

- [ ] **P2-1. `cacheMaxSizeMB: 0` disables eviction AND compaction with no residual bound** — `project.rs:204`; extension sends the raw value unclamped (`config.ts:38`, `nativeTransport.ts:717`), daemon doesn't clamp; package.json minimum (64) guards the settings UI only, not hand-edited JSON. Old main's unconditional 20k-entry cap is gone. (Registry retention still runs.) Fix: clamp at Hello.
  - **DV: ✅ CONFIRMED.** No daemon-side clamp of `cache_max_size_mb` at `project.rs:94`; the `package.json` `minimum:64` only guards the settings UI, not a hand-edited `settings.json`. Shared root cause with P2-7.
- [ ] **P2-2. Dirty-set memory-cap arithmetic** — `memory.rs:714-722`: candidates exclude dirty keys but `excess` uses full map length; a large dirty set (needs persistent `disk.insert` Err — serialization failure or poisoned lock) evicts all clean entries while the map grows unboundedly, re-running the O(n log n) scan per insert.
  - **DV: ✅ CONFIRMED** (needs a persistent `disk.insert` Err to accumulate the dirty set).
- [ ] **P2-3. `unknown_retry` leak + inherited graduation** — `memory.rs:577/600-606`: entries removed only by clear()/same-key non-Unknown read/re-insert; a key evicted mid-graduation leaks, and the evict→rehydrate→new-Unknown path inherits stale `first_seen` → first sighting flashes Unverified, skipping the §4.3.1 grace window.
  - **DV: ✅ CONFIRMED (both parts) — low.** The leak is bounded HashMap growth (eviction paths call `memory.remove` without `clear_unknown`). The inherited-graduation flash requires the extra coincidence of an `Unknown` disk-rehydration on the leaked key before any Fresh/Stale/re-insert clears it (`memory.rs:365` raw insert with no clear → `record_unknown`'s `or_insert` reuses stale `first_seen`/`attempts`).
- [ ] **P2-4. Registry failure-path write-back resurrects a pre-clear entry** — `registry/service.rs:489-494/716-727`: `failed_entry_from_cache(self.cache.get(...))` snapshots the pre-clear entry and can `write_entry` it after `clear()` (the fetch-success path at `:448` writes fresh data — acceptable; the failure path re-persists old data).
  - **DV: ✅ CONFIRMED — low (narrow race).** Requires a `clear()` to land between the failure path's `get` and `write_entry` during a failing fetch. Notable because it breaks the invariant `clear()`'s own comment relies on ("only fresh fetches re-seed"). In the common no-clear case it merely rewrites already-present metadata (harmless).
- [ ] **P2-5. mtime-0 sentinel defeats the pre-filter on mtime-less filesystems** — `key.rs:227-234/246-247`: stored 0 == current 0 + equal len → Fresh with no hash on non-strict paths (graph cache, node_modules deps) for exotic FUSE/network mounts.
  - **DV: ✅ CONFIRMED** (low — exotic mtime-less mounts only; first-party uses the strict check).
- [ ] **P2-6. Specifier-fallback merge collapses same-specifier imports** — `refreshMerge.ts:67-87`: legacy fallback keys by specifier (last wins, applied to all states with it). Only reachable under new-extension/old-daemon version skew — current daemon always sends aligned identities (`service.rs:955-960`).
  - **DV: ✅ CONFIRMED as latent / skew-only.** Collapse is real but unreachable with the current daemon (identities always present + index-aligned → `useIdentity` true). Correctly scoped.
- [ ] **P2-7. `budget_bytes == 0` renders "X MB / 0 B, 0 B free"** — `cacheManagerItems.ts:81-100`: `??` keeps 0 ("budget disabled" per `protocol.rs:783-786`); reachable only with the out-of-schema config from P2-1.
  - **DV: ✅ CONFIRMED** (display trap; same root as P2-1).

---

## Rejected / reclassified on double-verification

*Findings whose stated severity did not survive the second independent pass. The underlying code observation may still be real — the rejection is of the **classification**, with the reason recorded so it is not silently re-escalated.*

<a id="rb-6-reclassified"></a>

### RB-6 (was P0) → reclassified P2, PARTIAL — "prefetch window is huge / entries probe Fresh" not upheld
- **Original claim:** Generation snapshot taken AFTER resolution (`service.rs:1879`), so a `NodeModulesChanged` between resolution and analysis yields entries built from mismatched resolution metadata that probe Fresh, over a window "spanning the whole queued-job lifetime for prefetch."
- **What survives:** The *structural* observation is true — `captured_generation = cache_generation()` sits inside `analyze_and_cache` on an already-resolved package, and prefetch reuses `job.resolved.clone()` with no per-job re-resolve (`prefetch.rs:263`). Capture-after-resolve is real.
- **Why the P0 impact is rejected (two mitigations the review omitted):**
  1. `NodeModulesChanged` calls `prefetcher.cancel()` (`server.rs:714-716`), and jobs re-check `cancellation.is_current` at `prefetch.rs:259` / `should_continue` before running — so in-flight jobs resolved-but-not-yet-run **bail** rather than analyzing against stale metadata. The "window spans the whole queued-job lifetime" premise doesn't hold.
  2. The analyzed bytes **are content-hashed at read time**, so the stored fingerprints describe the current bytes regardless of when the generation was captured.
- **Residual (the real, downgraded finding):** a narrow check-vs-capture TOCTOU that can only mis-stamp *resolution-derived metadata not present in the fingerprint set* — `is_cjs`, `side_effects` — because the generation bump precedes the cancel in the handler. The interactive path is a microsecond same-thread gap. This is a P2-class latent correctness nit, not a permanent-stale release blocker.
- **Action:** treat as P2. If the freshness-core work (RB-1/2/4/5) captures the generation before resolution anyway, this closes for free; otherwise no independent blocker work is warranted.

*No other RB / P1 / P2 finding was rejected — all 15 remaining P0s, all 13 P1s, and all 7 P2s reproduced against `3038a15` at (or near) their stated severity, with the per-finding `DV:` refinements above.*

---

<a id="quality-debt"></a>

## Quality debt (from the cleanup angles; not adversarially verified)

**Duplication / drift risk**
- [ ] `read` vs `read_with_result_freshness`: two parallel ~130-line probe→verify→restamp→hydrate machines (`memory.rs:395` area; e.g. 293-313 vs 458-476). Fold into one core returning `(ImportResult, ResultFreshness)`.
- [ ] The `first_party ? strict : lenient` freshness-gate dispatch is copy-pasted at five sites (`disk.rs:255`, `disk.rs:347`, `memory.rs:262`, `memory.rs:422`, `memory.rs:619`) — hoist into one `CachedImport` method. `pending_insert_entry` duplicates `get_entry`'s whole gate (`disk.rs:341`).
- [ ] `check_fingerprints` vs `check_fingerprints_strict` duplicate the worst-of fold (Unknown>Gone>Stale>Fresh, `key.rs:264-315`); the `/node_modules/` substring party-test also lives at `service.rs:2065`. One parameterized fold + one shared predicate.
- [ ] Four near-identical clear actions in `cacheManager.ts:179+` (confirm→progress→remove→report skeleton) — one parametrized helper.
- [ ] `ReadIntent` name collision (service vs cache::memory) + separate `serve_stale: bool` threaded through five functions encoding one three-state mode — one `CacheReadMode` enum.
- [ ] Test fixtures: five hand-copied `ImportResult` builders and several ad-hoc temp-dir helpers (some without uniqueness suffixes — rerun flake risk: `cache_identity.rs:29/54`, `freshness_core.rs:11`) — hoist to `tests/common`.
- [ ] `stat_token` (`file_size_cache.rs:242-248`) derives mtime differently from canonical `key.rs:231-233` (no u64 clamp). No effect today (compared only against itself) — drift risk only.

<a id="cleanups"></a>

**Dead code / residue**
- [x] `CachedImport.size_bytes` is written but never read by production code; `DiskCache::insert`'s `u64` return exists only to feed it. *(✅ removed `53f5b15`; `insert` now returns `Result<(), String>`)*
- [x] The extension-side `cleanupCache` RPC chain is caller-less across five files (`manager.ts:106`, `transport.ts:185`, `nativeTransport.ts:510`, IpcClient pending map + guard) — delete. *(✅ deleted `8f20cfc`)*
- [x] ~~`purge_orphans` survives as a third destructive reclaim engine — retire it.~~ **Reversed 2026-07-08 (➖ decided: kept):** `purge_orphans` is now a wanted feature — see [RB-17](#rb-17-orphaned-project-cache-shards-are-never-proactively-reclaimed--purge-must-be-drive-safe). Keep it; make it drive-safe + automatic + re-add the button. Do NOT retire.
- [ ] Deprecated `cache_max_age_days` threaded through both registry constructors, stored, and echoed in stats (`project.rs:111`, `protocol.rs:771`) as if live — absorb at the protocol boundary.
- [ ] `MAX_EVICTION_SCAN` vs `EVICTION_BATCH` invariant held by comment (`memory.rs:871`) — make it `n.max(MAX_EVICTION_SCAN)` or assert.

**Efficiency (hot paths)**
- [ ] First-party entries re-read + xxh3-hash their entire graph on EVERY get (`memory.rs:263` strict path bypasses the TTL); a short verified-hash memo or hash-on-prefilter-mismatch with periodic strict backstop keeps X-7 at a fraction of the cost. Big cost in monorepos.
- [ ] `check_fingerprints` has no per-request memo: 30 imports of one package re-stat the same dep list per import (`key.rs:264`) — request-scoped (path → Freshness) memo.
- [ ] `DiskCache::insert` deep-clones the whole `CachedImport` just to set `cache_hit=false` (`disk.rs:284`); the recency sweep re-encodes multi-KB envelopes to update an 8-byte seq (`memory.rs:962`) — a `promote_seq(key, seq)` splice; the Fresh restamp clones the entry to write two stamps (`memory.rs:304`) — atomic cells like `last_seq`.

---

## Refuted (verified false — do not re-chase)

> **DV spot-check:** 5 of the 12 rows below were independently re-verified in the double-verification pass (marked **✔ DV-agree**); all 5 were correctly refuted — no real bug was buried under a false dismissal.

| Claim | Why it's wrong |
|---|---|
| Windows URI casing drops SWR pushes (`extension.ts:377`) | vscode-uri canonicalizes both sides identically (`Uri.file(fsPath).toString() === uri.toString()`, incl. uppercase drives/UNC); non-file schemes never reach the daemon (`listener.ts:74`). |
| v3 keys immortal after the v4 bump (`key.rs:162`) | **✔ DV-agree.** Key bump shipped with schema v5 (now v7); any pre-upgrade shard fails the version check on open and is wiped wholesale (`disk.rs:44-53, 1118-1151`; recreate-on-mismatch confirmed). |
| Generation bump must precede store mutation (`service.rs:1559`) | Trailing bump + captured-at-read generation forces re-validation of any in-flight insert (`memory.rs:645-655, 249-266`); residual scan-window lag is bounded and self-healing. |
| Temp-open collision permanently degrades a project to memory-only (`project.rs:462`) | The degraded cache is deliberately NOT registered (`project.rs:339-348`); next call retries the open. (Temp-opens do skip the load lock, but the consequence is per-call.) |
| `shard_rollup` empty() hides drifted bytes (`disk.rs:476`) | Open-time heal rebuilds SUMMARY from a full scan whenever row count ≠ entry_count; entry_count==0 with real bytes is unreachable (`disk.rs:848, 882-894`). |
| Serve-on-Unknown reports stored `fresh()` to clients (`memory.rs:288`) | **✔ DV-agree.** Plain `get()` is only an existence check; client-visible paths go through `read_with_result_freshness`, which graduates Unknown to Stale/Unverified — never a bare `fresh()` (`memory.rs:446-456`). |
| Dirty entry's old disk row evictable → only copy lost (`memory.rs:900`) | **✔ DV-agree.** Insert stamps `last_seq`/`persisted_seq` to the same fresh `born_seq` BEFORE the disk attempt; the old row's smaller seq makes the entry hot-shielded (`memory.rs:661-670, 884-886`). Newer value survives in dirty memory and is replayed by flush. |
| `last_seq.store` (not fetch_max) makes a just-used entry a victim (`memory.rs:241`) | **✔ DV-agree.** `next_seq()` is a process-global monotonic counter, so even a racing `store` writes a very-recent high value; worst case is one recency tick below the max — nowhere near eviction range. |
| Uncanonicalized symlinks → first-party misclassified (`key.rs:88`) | `normalize_identity_path` canonicalizes at key-build (`key.rs:404-406`, test `:483-492`); the canonicalize-failure residue is documented and TTL-bounded. |
| Bulk-refresh collector index can panic (`server.rs:628`) | **✔ DV-agree.** Index is `enumerate`-derived over the same vec whose length sizes the buffer (`server.rs:626` `vec![None; target_count]`) — in range by construction. |
| Registry toast overclaims on partial failure (`cacheManagerItems.ts:143`) | `failed>0` is unreachable for the registry scope (daemon returns no shard results, `service.rs:1507-1513`); hard failures surface via `response.error` first. Latent only. |
| `stat_token` vs `modified_millis` derivation divergence has an effect | Differs only above u64::MAX millis (~585M years); tokens are never compared across derivations. |

---

## Suggested fix order

**✅ Done (2026-07-08):** RB-12 (`c2dffbe`), RB-1 (`6eddb2f`), RB-2 (`e6c98ac`), RB-4 (`83826e2`), RB-5 (`204e303`), RB-3 (`5b4cdfc`), RB-17 + RB-7 (`354d297` / `6551ce9`), P1-12 (`800823e`), plus the dead-code cleanups (`8f20cfc` / `53f5b15`). Maintenance is now one pass **per project-open**, not a recurring 60 s tick (`3500d64`). The freshness core (RB-1/2/4/5) and clear-integrity trust failure (RB-3) are closed; RB-3 was adversarially reviewed (opus).

**▶ Active sequence: none in flight.** The next batch is the remaining clear/lifecycle-integrity items, then budget honesty, then SWR/registry UX — pick up when directed.

**Remaining, in order:**
1. **Clear/lifecycle integrity (finish):** RB-8 (load-lock in `remove_shard_by_id`), RB-10 (per-entry flush errors, never abort the shard loop).
2. **Budget honesty.** Split into *decided* vs *still-open* — do NOT read the whole cluster as optional:
   - **Decided, no code (only-if-needed):** **RB-9** is a deliberate *bounded-overshoot* tradeoff (per-open enforcement, ~2× worst case, self-correcting) — not a bug. The synchronous `evict_if_over_budget` on the flush path is the escalation we'd add *only if* that overshoot ever proves too loose.
   - **Still-open real bugs (to do):**
     - **RB-11** — eviction progress guard uses `entry_count`, not bytes freed, so a sustained burst falsely retires the over-budget shard as "exhausted" and the budget goes unenforced. Fix: judge by `freed > 0`. Small.
     - **RB-15** — physical/logical/per-shard-ratio mismatch → a pass that temp-opens every shard and evicts/compacts nothing; physical footprint never tracks the budget. `3500d64` cut the *frequency* (per-open) but not the logic bug.
     - **P2-1** — `cacheMaxSizeMB: 0` (hand-edited JSON past the UI's `minimum:64`) disables eviction **and** compaction with no residual bound. Fix: clamp at Hello. Drags the **P2-7** display trap (same root).
3. **SWR/registry UX:** RB-13 (filter error pushes off SWR), RB-14 (SWR-scoped supersession), RB-16 (wire `registryCacheMaxSizeMB` through Hello), then the P1 tail.
