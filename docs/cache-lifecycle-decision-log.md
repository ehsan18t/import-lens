# Cache Lifecycle — Decision Log

Chronological record of the design and scope decisions for the cache-lifecycle
redesign (branch `redesign/cache-lifecycle`). This is the companion to the review
backlog (`superpowers/plans/2026-07-08-cache-lifecycle-review-backlog.md`, which
tracks the findings); this file tracks the **decisions**. Keep it updated as new
decisions are made.

Each entry: **Context** (why it came up) → **Decision** → **Rationale** →
**Consequences / status**, with the commits and related findings.

---

## Standing policies

- **No deprecated / back-compat code.** The app is unreleased, so we delete stubs
  and legacy paths outright rather than keeping them for wire/settings
  compatibility. (2026-07-08)

---

## Decisions

### D1 — Remove dead / deprecated cache code (three cleanups) · 2026-07-08
- **Context:** The redesign was almost entirely additive (~6.7 K net production
  lines added, <1 K removed); a chunk of old machinery was left alongside the new,
  which also made the diff hard to review.
- **Decision:** Delete the genuinely dead / deprecated code now; keep the bug-fix
  work separate.
- **Rationale + verification (confirmed non-load-bearing before deleting):**
  - `8f20cfc` — dead `cleanupCache` RPC chain (no production sender) + its orphaned
    `last_cleanup_millis` status field. **Periodic maintenance is unaffected** — the
    tick calls `run_maintenance(false)`; only the unreachable manual `cleanup()`
    wrapper (`run_maintenance(true)`) went.
  - `800823e` — deprecated `cacheMaxAgeDays` end-to-end, incl. the `package.json`
    declaration (no deprecation stub kept, per the standing policy). **Closes P1-12**
    (editing it no longer bounces the daemon).
  - `53f5b15` — write-only `CachedImport.size_bytes` field. **Budget enforcement is
    unaffected** — the limit sums `ShardRollup.total_bytes` (the incrementally
    maintained SUMMARY scalar), never that per-entry field.
- **Status:** Done. All three verified green (`cargo test --workspace`, `tsc`,
  biome, TS tests).

### D2 — Orphan reclaim is a first-class feature; do NOT retire it · 2026-07-08
- **Context:** The redesign had marked the orphan-cache purge "deprecated / retire
  in Part F" and retired its UI button. But it addresses a real gap the owner cares
  about: a project that is **moved or deleted** is never reopened, so the on-access
  reclaim (name invalidation + freshness `Gone` eviction) never reaches it and its
  whole cache shard lingers — worst for a *recently* used project, which the LRU
  byte budget evicts last.
- **Decision:** Reverse the retirement. Keep orphan reclaim, make it **drive-safe**
  and **automatic**, and **re-add the manual button**. (An in-progress removal of
  the feature was reverted.)
- **Rationale:** Entry-level staleness in *live* projects is already handled
  automatically; the uncovered case is a whole **abandoned-project shard**, which
  nothing scans for. The old scan was also unsafe (RB-7): a Windows unplugged drive
  reports `ERROR_PATH_NOT_FOUND` (→ `NotFound`), so it could destroy a valid shard.
  The fix is to make it safe, not to delete it.
- **Consequences / status:** Done — see [RB-17](superpowers/plans/2026-07-08-cache-lifecycle-review-backlog.md#rb-17-orphaned-project-cache-shards-are-never-proactively-reclaimed--purge-must-be-drive-safe).
  - `354d297` — daemon: `classify_project_root` (Present / Orphaned /
    VolumeUnreachable) so an offline drive keeps its shard (**closes RB-7**);
    drive-safe `purge_orphans`; throttled `sweep_orphaned_shards_if_due` on the
    maintenance pass; unit + integration tests.
  - `6551ce9` — extension: Manage-Cache "Remove Orphaned Caches" action.

### D3 — Cache maintenance runs once per project-open, not on a recurring 60 s tick · 2026-07-08
- **Context:** The maintenance pass ran every 60 s per connected window, which felt
  wasteful. (It does: byte-budget eviction + compaction, registry 30-day retention +
  size cap, and the orphan sweep.)
- **Decision:** Replace the recurring tick with a **single pass scheduled 60 s after
  Hello** (once the cold-open analysis burst has settled), then stop. Each new
  project-open schedules its own pass.
- **Rationale:** A project's cache **converges** to its distinct-import footprint —
  re-analysis is a cache hit, not growth — so it cannot grow unboundedly over a
  session; continuous polling is wasted work. Multi-project growth is still bounded
  because every open triggers a global pass.
- **Consequences / status:** Done (`3500d64`).
  - **Accepted tradeoff:** a heavy long *single*-project session may sit up to ~2×
    the budget until the next open/relaunch — bounded, cheap, self-correcting.
  - **Reframes RB-9:** enforcement is now per-open, not a 60 s window; bounded
    overshoot is a deliberate choice over synchronous insert-path eviction (which
    stays the clean fix if the overshoot ever proves too loose).
  - **Defangs the every-60 s symptoms of RB-15** (idle shard-scan) and **P1-10**
    (registry snapshot rewrite) — they now fire at most once per open.

### D4 — Registry `Retry-After` is clamped, not honored verbatim · 2026-07-08
- **Context:** A registry `429` may carry a `Retry-After` the server picks; honoring a
  huge value verbatim parks a connection-pool slot for that whole duration, and a hostile
  or buggy header (`Retry-After: 999999`) could wedge the pool indefinitely.
- **Decision:** Clamp the honored delay at `REGISTRY_MAX_BACKOFF_MS` (5 min). We do NOT
  fail-fast/cancel the sleep, and we do NOT honor the raw value.
- **Rationale:** The clamp alone removes the unbounded-wedge harm while still backing off
  a real 429 for a meaningful window; 5 min is longer than any legitimate transient rate
  limit needs and short enough that a bad header can't strand a slot. Fail-fast would
  throw away a legitimate backoff signal.
- **Status:** `c2dffbe` (RB-12). A capped-not-verbatim `Retry-After` is deliberate — not a
  dropped-header bug.

### D5 — Module-graph reuse gate: strict hash, but reuse on transient `Unknown` · 2026-07-08
- **Context:** The `GRAPH_CACHE` reuse gate re-checks a cached graph's fingerprints before
  serving it. Two independent choices live here.
- **Decision:** (a) Use `check_fingerprints_strict` (content-hash-verify first-party
  modules) on BOTH reuse gates, not the cheap mtime+len pre-filter. (b) On a tri-state
  `Unknown` (a transient stat/read error), REUSE the cached graph; rebuild only on a
  definite `Stale`/`Gone`.
- **Rationale:** (a) L2 recomputes *through* this cache, so a first-party edit that the
  mtime+len pre-filter misses (equal-length, mtime-preserving) would be served stale
  forever — the strict hash closes that. (b) A momentarily-locked file must not force a
  rebuild storm; serving the last-known graph while the error is transient mirrors L2's
  stale-while-revalidate contract. Reuse-on-`Unknown` is therefore intentional, not a
  staleness hole.
- **Status:** `6eddb2f` (RB-1).

### D6 — First-party CJS freshness via a cached module set, not a short TTL · 2026-07-08
- **Context:** The CommonJS analyzer walks and reads every transitive `require()`, but
  produced no `ModuleGraph`, so a first-party (workspace/`file:`/npm-link) CJS dep was
  unfingerprinted in both L2 (`dependency_fingerprints`) and L1
  (`first_party_module_token`) — a deep-module edit never invalidated.
- **Decision:** Cache the CJS walk's module set (canonical paths + read-time content-hash
  fingerprints) in a bounded LRU `CJS_MODULE_CACHE` mirroring `GRAPH_CACHE`, peeked by L1
  (paths) and L2 (fingerprints). Deliberately NOT a short-TTL/force-probe on the entry
  alone. The set is cached for every completed walk, including one the caller later rejects
  to static-entry sizing.
- **Rationale:** A short TTL on the entry would miss the transitive deps entirely (the
  actual gap) and reintroduce time-based staleness the redesign removed. The cache reuses
  the bytes already read during the walk, so it adds no I/O. Populating on the
  static-fallback path is the deliberate call: the module set is then unused or yields
  slight L2 over-coverage (an extra recompute on a transitive edit) — never a stale-serve —
  and gating it would add branchy special-casing for no correctness gain. So the
  populate-always and the occasional over-coverage are intentional, not over-invalidation
  bugs.
- **Consequences / status:** `204e303` (RB-5). Cleared/invalidated/purged on the same seams
  as the graph cache. Also closes the RB-2 residual for CJS (the manifest/entry re-stat's
  narrow TOCTOU): first-party CJS modules now carry read-time hashes; the ESM path already
  did.

### D7 — Clear-race guard is an optimistic generation, not a hot-path lock · 2026-07-08
- **Context:** `clear()` ("Clear cache") races concurrent cache writers, which could
  resurrect just-cleared entries (RB-3). The clean textbook fix is one lock spanning the
  wipe and every writer commit — but inserts run on the parallel cold-analysis hot path
  and are deliberately lock-light (a batched pending queue), so a global insert lock would
  serialize exactly what the redesign parallelized.
- **Decision:** Guard with a `clear_generation` counter instead. Writers tag their queued
  bytes / capture the generation before deriving them; `clear()` bumps it and the flush
  drops any entry whose tag is stale. Only the rare `clear()` and the batched flush take a
  lock (`clear_lock`); the per-insert path stays lock-free apart from one atomic load.
- **Rationale:** The generation makes a stale write self-identify, so correctness needs
  mutual exclusion only between `clear()` and the flush — not on every insert. Two
  `disk.insert` variants exist by design: `insert` tags the CURRENT generation (fresh,
  derive-now bytes), `insert_at_generation` tags a caller-captured one (snapshot-derived
  writers: `flush_to_disk`, `enforce_memory_cap`) — not redundancy.
- **Accepted residual (so it is not later filed as a bug):** the memory-side guard
  (`insert_into_memory_guarded`) is optimistic — pre-check, insert, then an
  identity-checked (`Arc::ptr_eq`) rollback. It never resurrects a cleared entry and never
  serves stale data; the only residual is an astronomically-rare interleaving (a writer's
  pre-check passes, then a clear AND a concurrent same-key insert both land before its
  insert) that can drop that one entry to a **cache miss** — self-healing (recompute; on a
  disk-enabled cache the disk copy re-hydrates). Chosen over a hot-path lock or a
  per-attempt clone in `papaya::compute`. Adversarially reviewed (opus): no resurrection
  window, no deadlock (`clear_lock → db → pending` order; `clear_lock` never re-entered).
- **Consequences / status:** `5b4cdfc` (RB-3).

### D8 — SWR pushes a refreshed size only when the cache would accept it · 2026-07-08
- **Context:** RB-13 — a stale-while-revalidate recompute could push an error/degraded
  result over a good last-known size. The finding offered two fixes: filter *errors*, or
  "push only when the cache also accepts them."
- **Decision:** Gate the SWR push on the SAME `should_cache_result` predicate the cache
  write uses (daemon `revalidate_document_sizes`) — so it drops BOTH hard errors AND
  request-specific diagnostics (e.g. a default import of a named-only package → missing
  export). The client-side merge keeps a weaker `error === null` filter.
- **Rationale (so this is not misread as a bug):** coupling push⟺cacheable means display
  and cache can never disagree (the exact RB-13 harm). Dropping diagnostic-carrying results
  from the *proactive* push is deliberate, not an oversight — they are still shown on the
  next interactive `file_size_document` (not cached → recomputed), so nothing is hidden;
  the SWR badge just doesn't chase them. The daemon/client filter asymmetry is intentional:
  the daemon is the authoritative filter (the client never receives a diagnostic result to
  mishandle), and the client's `error === null` is redundant defense-in-depth for other
  push paths / an older daemon — the client can't cheaply replicate
  `has_request_specific_diagnostics`. *Test note:* the pre-existing same-specifier-variants
  test was reconciled (its fixture gained a real default export) because its default variant
  is now correctly filtered; the identity-alignment purpose is preserved and the filtered
  case is covered by a new dedicated test.
- **Consequences / status:** `df1eacb` (RB-13).

### D9 — SWR supersession is per-document, accepting redundant cross-document recompute · 2026-07-08
- **Context:** RB-14 — SWR revalidation was starved by the prefetcher's *global* cancel
  token (any unrelated message cancelled it) and by a per-cache-key in-flight claim (a
  second document sharing a package got no push).
- **Decision:** Give SWR its own per-document cancel token (`SwrRefreshLifecycle`, keyed by
  workspace+document) and make the in-flight claim document+generation-scoped, not the bare
  cache key.
- **Rationale (so these are not misread as bugs):** (a) two different documents importing
  the same package now BOTH recompute it — deliberate redundant CPU traded for
  anti-starvation; the global cache key means the *result* is still shared, only the compute
  is duplicated, and genuinely identical work (same key+doc+generation) still coalesces.
  (b) `active_by_document` accumulates one small entry per document opened per connection and
  is freed on disconnect — a bounded, accepted footprint, not a leak (a `strong_count == 1`
  sweep could prune it if it ever matters). (c) `start_document` fires on every
  `FileSizeDocument`, so a fresh read of a document cancels that document's own older
  refresh even when it finds nothing stale — correct supersession, not an over-cancel.
- **Consequences / status:** `df1eacb` (RB-14).

### D10 — `registryCacheMaxSizeMB: 0` means "no cap", not "evict everything" · 2026-07-08
- **Context:** RB-16 wired `registryCacheMaxSizeMB` through Hello, making the value live
  end-to-end (the old code always used the hardcoded 32 MiB constant). A hand-edited `0`
  (out of the package.json schema) now reaches the registry evictor.
- **Decision:** Guard `evict_oldest_over_budget` so `max_bytes == 0` evicts nothing (the
  store is uncapped), matching the main byte budget's `budget_bytes == 0` early return —
  rather than draining the whole hint store every maintenance pass.
- **Rationale:** 0 as "disabled" is consistent with the main budget and is the safe reading
  of an out-of-schema value; a proper minimum-clamp at Hello is the shared P2-1 follow-up
  (parked). Surfaced by the RB-16 adversarial review.
- **Consequences / status:** `df1eacb` (RB-16).

### D11 — Bulk registry-refresh supersession is per-source manifest, mirroring D9 · 2026-07-08
- **Context:** P1-5 — the redesign (`610f27d`) added `RegistryRefreshLifecycle` with a single
  *connection-global* cancel slot: any new bulk refresh cancelled the prior block. A workspace
  prewarms several manifests (web + backend + `.next` `package.json`), each firing its own
  bulk block. On a **cold cache** the web block's network fetches are still in flight when the
  backend block starts, so it flips web's cancel flag; every not-yet-fetched web target reports
  `None` and the collector backfills a fabricated `"registry refresh worker did not return a
  result"`. The client keys generations per (uri, target), so those errors are *not* seen as
  superseded — they surface as real "hint unavailable", and the manifest renders no versions.
  Warm cache hid it (the web block finished before the backend block arrived).
- **Decision:** Key `RegistryRefreshLifecycle` **per source manifest** (a `HashMap<source,
  flag>`, exactly mirroring `SwrRefreshLifecycle` / D9). A refresh supersedes only the prior
  block for the *same* source; other manifests keep draining; connection-drop still cancels all.
  Threaded a new optional `source` field (the client's document key) through
  `RefreshRegistryHintsRequest`; absent source (older peer) shares one empty-key bucket,
  preserving the pre-D11 connection-global behavior for it.
- **Rationale:** identical shape to D9 — the sibling SWR path already made exactly this
  per-document move; registry refresh was simply left per-connection. The `active_by_source`
  map is one small entry per manifest per connection, freed on disconnect (bounded, not a leak).
- **Consequences / status:** fixes P1-5. Tests: daemon `registry_refresh_lifecycle_supersedes_
  only_within_the_same_source` (cross-source isolation + same-source supersede + drop-cancels-all)
  and extension `refresh sends the document key as the request source`.

### D12 — Post-cutover fingerprints re-read loaded paths after analysis; the analysis-time-hash guarantee is retired with the graph · 2026-07-11
- **Context:** The bundler-redesign Phase 3 cutover deleted the custom module graph, whose
  analysis-time content hashes let `dependency_fingerprints` pair a result with the exact
  bytes the analysis read (the old "Finding 4" TOCTOU guard and its
  `analyze_and_cache_fetches_module_graph_once` test died with it). The Rolldown engine's
  public output exposes loaded paths but not load-time content hashes, so fingerprints are
  now computed by re-reading each loaded path *after* the build.
- **Decision:** Accept the re-opened window (a dependency edited between engine load and
  fingerprinting can pair a stale result with fresh-looking fingerprints) rather than
  re-hashing inside the plugin's load path.
- **Rationale:** the window is milliseconds-to-seconds per import and requires an edit to a
  transitively loaded file exactly inside it; watched `node_modules` changes still invalidate
  via the pre-analysis generation gate, and the L1/L2 re-verify TTLs bound residual staleness
  to one window. Hashing in the plugin would double file reads on every build to close a
  race that eviction already bounds. Related: the old first-party CJS cached-module-set
  freshness (D6) is superseded, not lost — the engine failure fallback measures only the
  entry file, so its manifest+entry fingerprints now cover exactly the inputs of the cached
  computation.
- **Consequences / status:** lands with the Phase 3 cutover commit. Also removed in the same
  commit: the named→namespace cache alias (`cache_full_variant_alias`), whose premise —
  side-effectful packages size identically for named and namespace imports — was only true of
  the old engine's conservative full-graph inclusion; Rolldown shakes pure unused exports
  under `sideEffects: true`, so each import kind now computes its own entry.

### D13 — A transiently-degraded result is never cached, but IS pushed · 2026-07-13
- **Context:** the streaming redesign (`AnalyzeDocument` answers from cache and pushes each
  import as its build lands) deleted the per-request engine budget. Both the budget's
  abandoned builds and a real `BUILD_TIMEOUT` / `panic` / `engine_gone` leave the pipeline
  with only the conservative static fallback — which carries `error: None` and a plausible
  byte count, and so passed the old `should_cache_result` (`error.is_none()`) unchallenged.
  That is how one parked build taught the cache that a healthy 17,550-byte package weighs 58
  bytes, for a whole cache generation.
- **Decision (cache):** every cache and memo gates on the failure STAGE, not merely on
  `error`. `engine::stage::is_transient` (`timeout` | `panic` | `engine_gone`) is the single
  source of truth; `should_cache_result` and `FileSizeComputation::is_cacheable` consult it,
  covering L1 memory, L2 disk, and the L1 aggregate file-size cache. The full-package and
  export-list build memos and the dependency-path index need no gate — they are written only
  on the success path. A DETERMINISTIC failure (`parse`, `link`, `module_graph_limit`, a
  minify failure) is still cached: it is a fact about the code, and re-deriving it costs a
  full build to learn the same thing. The full-package comparison now reports its failure
  under the stage it failed at rather than a `full_package_comparison` literal, which is both
  what §12 asks for and what lets this gate see it — a `truly_treeshakeable: false` produced
  by a build that merely parked is as fabricated as a fabricated size.
- **Decision (push) — amends D8:** D8 coupled the SWR push to cacheability so display and
  cache can never disagree. That coupling holds for SWR and is now strictly stronger (a
  transiently-degraded revalidation is dropped, so a good stale size on screen survives a
  parked rebuild). It deliberately does NOT hold for the streaming push: an import the
  response answered `loading` has no previous value to protect, and withholding its result
  would leave it reading "Calculating…" for the rest of the session. So the streaming push
  delivers whatever the build produced, and the client-side merge carries the distinction
  instead — an errored/degraded result may FILL a state with no result, and may never REPLACE
  one that has (`mergeRefreshedResults`).
- **Consequences / status:** the degraded value is displayed (low confidence, with its stage
  diagnostic) but is not persisted, so the next read of that import recomputes rather than
  serving the accident back. Lands with the streaming-redesign commit; SRS FR-026c.

### D14 — The transience gate extends to the EXTENSION's persisted stores · 2026-07-13
- **Context:** D13 gated every cache the daemon writes. It did not gate the two stores the
  *extension* writes, because nothing had ever gated them: the import-cost history
  (`globalState`) and the bundle-impact history (`globalState`). Both are strictly worse
  places for a fabrication than any daemon cache — no TTL, no cache generation, not cleared by
  either Clear Caches command, and one row per identity. A daemon cache serves the accident
  back for a generation; these two *overwrite the import's real baseline for good*, and every
  later trend insight is then measured against a number that never happened. This is the same
  defect for the fourth time, in the fourth place.
- **Decision:** both stores gate on the daemon's own evidence, not on `error` (which a
  degraded result does not carry). `analysis/transience.ts` mirrors `stage::is_transient` and
  is the single funnel: `importCostHistoryItemsForStates` drops an import whose diagnostics
  name a transient stage, and `bundleImpactHistoryItemForResponse` refuses a total that is
  transiently degraded *or* `incomplete`. A drift check
  (`scripts/test/engine-stage-coordination.test.mjs`) fails the build if the Rust and TS stage
  lists disagree.
- **Decision (protocol):** `FileSizeDocumentResponse` gains `incomplete` on the wire.
  `FileSizeComputation::incomplete` already existed daemon-side (it is what keeps a floor out
  of the L1 aggregate cache), and the client cannot re-derive it: the diagnostic naming a
  still-`loading` import is stage-tagged `file_size_fallback`, exactly like the diagnostic for
  a deterministic per-import failure, which is a real fact and no reason to distrust the total.
  Additive, `#[serde(default)]`, no protocol-version bump.
- **Consequences / status:** an estimate is still shown ("· estimate"), with no delta against
  the previous run — comparing an honest total to a floor invents a regression — and is simply
  never written down. SRS FR-026c.

### D15 — Shutdown flushes under a bound; a build it cannot cancel is abandoned · 2026-07-13
- **Context:** shutdown joined every task the connection spawned before flushing. One class of
  task cannot be cancelled: a build already inside Rolldown runs to `BUILD_TIMEOUT` (8s). The
  extension force-kills the daemon 5s after sending `shutdown`, so the unbounded join
  *guaranteed* the kill landed first and `flush_cache` never ran — the graceful shutdown lost
  the session's unwritten cache to wait on one build whose result, if it timed out, D13 would
  have refused to cache anyway.
- **Decision:** cancel everything cancellable first (prefetch, registry blocks, streamed-import
  builds, SWR, queued combined builds, the scheduled maintenance pass — via explicit
  `cancel_all`, because `Drop` runs *after* the join it was meant to shorten), then join under
  `TASK_JOIN_TIMEOUT` (2s, comfortably inside the 5s kill), then flush unconditionally. A task
  still running at the deadline is abandoned and its result is recomputed next session.
- **Consequences / status:** the "no build is still writing to the cache after the flush"
  guarantee is downgraded to what is actually true and stated as such in FR-004c. Worst case, a
  late build writes to `papaya` after the flush and the process exits before it is persisted:
  one rebuild lost, against the whole session's cache saved.

---

## Parked / deferred

- **Back-compat vestige sweep** — audit for other deprecated/back-compat remnants
  (serde "older peers" defaults, log-and-skip comments), *excluding* the orphan code
  (now a wanted feature). Awaiting go-ahead.
- **Budget-honesty cluster** (RB-9 / RB-11 / RB-15 / P2-1) — lower urgency after D3
  (overshoot is now a bounded, deliberate tradeoff). The clean fix is a synchronous
  `evict_if_over_budget` on the flush path, if tighter enforcement is ever wanted.
