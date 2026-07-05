# Cache Lifecycle Redesign — Design Spec

> **Status:** Approved design, pre-implementation. Supersedes the open questions in `2026-07-05-cache-lifecycle-redesign-brief.md` (Part E). Evidence base: `2026-07-05-cache-lifecycle-validation-findings.md` (every claim + `X-n` issue referenced here is validated there against `main`).
>
> **One-line intent:** Replace ~7 overlapping, accreted cache mechanisms with **two clean layers** (Freshness / Capacity) over **independently-bounded, independently-clearable stores** — correctness first, then a single global size budget.

---

## 1. Goals & non-goals

**Goals**
- **Correctness first.** No cached bundle size is ever served as fresh when it was computed from different bytes. Kill the permanent-staleness bugs (D4, the TOCTOU X-1, the prefetcher race X-2) at the source.
- **One authoritative capacity bound.** A single global disk-byte budget with continuous LRU eviction that *actually* bounds on-disk footprint.
- **Best-in-class UX.** Stale-while-revalidate: users see an instant last-known answer, never a spurious "checking…" on tab switches or no-op touches. Content-addressed so undo/reinstall are cache hits.
- **Decoupled, honest stores.** Every cache is bounded, every cache is clearable, "Clear" clears what it says, and success messages don't overclaim.
- **Robust on Windows.** Transient stat/lock errors never destroy valid caches; clocks are monotonic.

**Non-goals**
- No third-party cache/LRU library — the budget is rolled on the existing `redb` shards.
- No stale-badge **UI** in this iteration (the freshness flag ships in the *data* layer only; UI/setting is a later, pure-presentation change).
- No re-architecture of the analysis pipeline itself (parser, tree-shaker, compressor) beyond the fingerprint-capture point.
- No preservation of existing on-disk cache contents across the upgrade (a v4 identity + schema change; one cold-cache moment is accepted — see §12).

---

## 2. Design principles

1. **Validity ≠ capacity.** "Is this entry still correct?" (Layer 1) and "do we have room?" (Layer 2) are separate concerns with separate code paths. They meet only at the entry record.
2. **One source of truth per fact.** Recency lives *inside* the entry (no side tables). Bytes are summed from the shards (no divergent ledger). Generation is captured at analysis start (not stamped at insert).
3. **Content-addressed validity.** A result is bound to a hash of the exact bytes that produced it — not to a timestamp.
4. **Fail safe, not destructive.** Unknown/transient filesystem state means *keep*, never *evict*. Only a definitive `NotFound` deletes.
5. **Automatic maintenance, explicit destruction.** Capacity eviction and validity reclaim run automatically and continuously; users only ever click to *deliberately clear* a chosen scope.

---

## 3. Architecture overview

### 3.1 Two layers, three-plus stores

```
┌──────────────────────────────────────────────────────────────────────┐
│ LAYER 1 — FRESHNESS (correctness)                                      │
│   FileFingerprint (content hash + mtime pre-filter)                    │
│   FreshnessProbe  → Fresh | Stale(changed) | Gone(NotFound) | Unknown  │
│   SwrRevalidator  (in-flight dedupe, background recompute)             │
│   Generation      (captured-at-read, coarse fast-path gate)            │
└──────────────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────────────┐
│ LAYER 2 — CAPACITY (single global byte budget)                         │
│   RecencyClock (monotonic u64)   BudgetCoordinator (single writer)     │
│   per-shard seq index + rollup   Evictor (hysteresis, per-proj floor)  │
│   Compactor (threshold-triggered redb compaction)                      │
└──────────────────────────────────────────────────────────────────────┘
   STORES (each independently bounded + clearable):
   • Bundle caches  — per-project redb shards      → GLOBAL BYTE BUDGET
   • Registry       — npm hints, separate file     → OWN size limit + retention
   • L1 / graph / resolvers — in-memory derived     → existing small caps
```

### 3.2 Data flow (analyze request)

```
resolve import → memory working set → FreshnessProbe(entry)
   Fresh            → serve (promote recency)
   Stale(changed)   → serve last value flagged Stale → SwrRevalidator (dedupe → bg recompute → push fresh)
   Gone(NotFound)   → drop → recompute
   Unknown          → serve last value (never evict) → retry; if persistent → Unverified (surfaced)
recompute → analyze_and_cache: snapshot(generation, per-file content hashes) BEFORE reading bytes
          → insert(entry{value, hashes, gen, bytes, last_seq})
          → BudgetCoordinator.record() → evict_if_over_budget()
```

### 3.3 Startup / events

- **Startup:** open shards → rebuild in-memory rollups from each shard's stored `total_bytes`/`oldest_seq` → run analyzer-version validity sweep → scan-resistant prewarm of most-recent entries.
- **`NodeModulesChanged`:** bump generation → invalidate affected packages by name → reclaim path-missing entries for that event.
- **Manage Cache (user):** scoped clear → clears the selected store(s) + bumps generation → returns accurate counts.

---

## 4. Layer 1 — Freshness (correctness)

### 4.1 Content-hash fingerprint (I-1)
- `FileFingerprint { hash: u64, mtime_millis: u64, len: u64 }` where `hash` is a fast non-cryptographic hash (xxHash3 or ahash — integrity, not security) of the file bytes **actually read during analysis**.
- **`mtime_millis`+`len` are a pre-filter only**, never the source of truth: on revalidation, if `mtime`+`len` are unchanged, skip re-reading (assume `hash` unchanged); if changed, re-read and re-hash to decide — a no-op touch (`npm ci`, editor save with identical bytes) re-hashes to the same value and is a **hit**.
- Consequence: closes the TOCTOU completely, removes the same-millisecond blind spot (X-7), and makes identical-content reinstall/undo cache **hits**.

### 4.2 Atomic capture (fixes X-1 TOCTOU, D4, X-2 prefetcher race)
- `analyze_and_cache` captures the freshness inputs **before/around the content read that produces the result**, not after:
  1. Snapshot `G_start = current_generation()`.
  2. For each dependency file: read bytes → hash them (the hash *is* the read).
  3. Compute the result from those exact bytes.
  4. Store `entry{ value, fingerprints = the hashes just taken, generation = G_start }`.
- **The stored generation is `G_start`, not the generation at insert time.** If a `NodeModulesChanged` bump lands during analysis, `entry.generation < current` at insert → the entry is already "must re-validate," and its content hashes reflect the *pre-change* bytes → the next probe finds a mismatch → Stale/Gone. A stale result can never be laundered into a fresh-looking entry.
- Applies identically to the two detached inserter paths (WorkspaceReport, Prefetcher) — they use the same capture, so their late inserts cannot bake in staleness.

### 4.3 Tri-state freshness probe (enables SWR safely)
`FreshnessProbe::check(entry) -> Freshness` (non-evicting):

| Result | Condition | Action |
|---|---|---|
| `Fresh` | all dep fingerprints current | serve; promote recency |
| `Stale(changed)` | a dep file differs but **still exists** | serve last value flagged stale; revalidate in background |
| `Gone` | a dep path returns **`NotFound`** (entry_path/package_root/graph file truly removed) | drop; recompute (never serve stale — recompute can't succeed until reinstalled) |
| `Unknown` | stat/read fails with a **non-`NotFound`** error (locked, AV scan, offline drive) | **keep** (never delete); serve last value **but retry with backoff**; if the error **persists**, surface it (§4.3.1) |

Only `NotFound` deletes. This single rule fixes X-3 (whole-shard deletion on a disconnected drive), X-4 (valid-entry eviction on transient error), and the file-gone-vs-file-changed hazard the brief flagged for D1.

#### 4.3.1 `Unknown` is honest, not silent
Serving a value we **couldn't verify** as if it were confirmed-current would mislead the user, so `Unknown` is *graduated* rather than silently served:
- **Transient (the common case):** a file locked for milliseconds by a save or AV scan almost always still holds the same bytes. **Retry with short backoff** and keep serving the last value *quietly* (marked `revalidating`) — we don't flash an alarming error on every filesystem blip.
- **Persistent:** if verification keeps failing past a small threshold (N retries / T seconds — a genuinely offline drive, a permission wall), **stop pretending.** Serve the last value but mark it **`Unverified{reason}`** and expose the reason through the existing `unavailable` / **Copy diagnostics** channel and hover — the same honest-staleness pattern the registry hints already use ("here's the cached value, and here's why we couldn't confirm it"). We still never *delete* (recompute would hit the same error) and never claim `Fresh`.

### 4.4 Generation model & the D3 window
- `CACHE_GENERATION` (monotonic `AtomicU64`) still bumps on node_modules invalidation and is the **cheap fast-path gate**, but it only decides *whether to re-validate*, never "serve as fresh":
  - **Fast path** (`entry.generation == current` **and** the entry has **no** first-party/linked deps): serve without re-stat. Safe because node_modules deps only change via install → a bump would break the equality; and atomic capture guarantees the fingerprints match the analyzed bytes.
  - **Slow path** (`entry.generation != current`, **or** a first-party/linked dep, **or** pre-filter trip): run the tri-state probe.
- **D3 fix:** first-party / linked deps (workspace, `npm link`, `file:`) **bypass the fast path entirely** — they are the only deps that change without a node_modules event, so they are always probed (cheaply, via the mtime pre-filter). SWR keeps this invisible to the user (§4.5).

### 4.5 SWR + in-flight dedupe + freshness flag
- `get_with_freshness(key) -> (value, Freshness)` never evicts.
- On `Stale(changed)`, the service returns the last value immediately and enqueues **one** background recompute per key via an **in-flight set** (`DashMap<Key, ()>` or equivalent): N imports of the same changed dep coalesce to one recompute; the fresh result is pushed when ready.
- Background recompute is **debounced** by the existing `importLens.debounceMs` (300 ms) and deduped, so active editing of a workspace dep yields one quiet recompute after typing stops — never a per-keystroke flicker.
- **Freshness marker in the data layer** (Adjustment 2), carried on every size result over IPC:
  ```rust
  enum ResultFreshness {
      Fresh,                               // verified current
      Stale { revalidating: bool },        // known-changed; recompute may be in flight
      Unverified { reason: VerifyError },  // couldn't confirm (persistent stat/read error); last-known shown
  }
  ```
  Reuses the same "stale" vocabulary as the existing registry `stale · …` hints (one staleness concept product-wide). No UI in this iteration; the flag makes a future `importLens.serveStale` setting or inline badge a pure-presentation change.
- **Internal probe → IPC flag mapping:** `Fresh → Fresh`; `Stale(changed) → Stale{revalidating: true}`; `Unknown` while **transiently** retrying `→ Stale{revalidating: true}` (rechecking quietly), and once the error **persists** `→ Unverified{reason}` (surfaced, §4.3.1); `Gone →` not served — triggers recompute, whose result arrives `Fresh`.
- **CI / CLI forces fresh.** The `importlens check` budget gate and any batch/CI analysis **bypass SWR** — they compute synchronously and never serve a stale or unverified value — so a budget pass/fail is always judged against the true current size.

### 4.6 Monotonic clocks (X-6)
All freshness TTLs / windows use `Instant` (monotonic), matching the registry rate-limiter — not `SystemTime`. A backward wall-clock jump can no longer extend a staleness window.

### 4.7 Cache identity v4 — drop in-key fingerprints (D2)
- `CacheIdentityV4` removes `manifest_fingerprint` + `entry_fingerprint` from the key. The key becomes stable across content-identical reinstalls; validity is enforced entirely value-side by §4.1–4.3 (which already covers package.json + entry + the full module graph, and is now *content-hash exact*).
- **L1 coupling (the one real dependency):** `file_size_signature` currently borrows mtime sensitivity from the key. It folds a document/content fingerprint into its own signature independently, so dropping key fingerprints doesn't weaken L1.
- Safe **because** §4.2 lands first — value-side re-validation is trustworthy before we start relying on it exclusively.

---

## 5. Layer 2 — Capacity (global byte budget)

### 5.1 Recency inside the entry + scan resistance (I-3)
- `RecencyClock`: one process-global monotonic `AtomicU64`. Each entry stores `last_seq`. Access promotes it to `RecencyClock.next()`.
- **This replaces `RECENTS_TABLE` + `pending_touches` entirely** — recency is part of the entry, so deleting an entry deletes its recency. Dangling rows become structurally impossible (kills 8f, X-8, X-9, X-10, X-12, X-13).
- **Scan resistance:** `get(promote: bool)`. Interactive reads promote; **bulk/background reads (WorkspaceReport, Compare, prewarm) do not promote**, so a full-workspace scan can't flood the recency signal or the working set.

### 5.2 Per-shard index + in-memory rollup (I-2, refines Approach C)
- Each shard gains a **by-`last_seq` secondary index** and stores its own `total_bytes` + `entry_count`.
- The daemon holds a small **in-memory rollup per shard**: `{ total_bytes, oldest_seq, entry_count }`, built at startup by reading each shard's stored summary (cheap; no full scan required in the common case, full scan only to heal a detected inconsistency).
- **One source of truth: the shards.** No separate persisted global ledger to drift; the rollup is a derived hint.

### 5.3 Evictor — hysteresis + per-project floor
- `BudgetCoordinator` tracks `global_total = Σ shard.total_bytes` against `cacheMaxSizeMB`.
- **High/low water:** when `global_total > budget` (high water), evict until `global_total ≤ budget × LOW_WATER` (e.g., 0.9) to avoid per-insert thrash.
- **Victim selection (exact-enough global LRU):** pick the shard with the smallest `oldest_seq` from the rollups → open its by-seq index → evict its oldest entries → update rollup → repeat across shards until under low water.
- **Per-project floor (3.4):** the evictor never evicts an entry within its shard's most-recent `FLOOR` entries (small, e.g., 128), so switching to a large project cannot evict a small project's warm set out from under the user.
- Runs continuously (after each insert-flush) and on demand — not just at startup.

### 5.4 Memory working set
- The in-memory mirror is a **count-bounded working set** (cache of disk). Evicting from memory drops only the map entry; disk remains durable under the byte budget. No data loss, no re-hydrate correctness issue.
- **`CACHE_RECYCLE_ENTRY_LIMIT` (200k) is deleted** — it operated on in-memory totals and was effectively dead (A7/E7). The two remaining knobs are *global disk bytes* and *memory working-set size*.
- **`enableDiskCache = false` degrades cleanly:** no shards, ledger, eviction, or compaction — a memory-only working set. Freshness + SWR still apply; the byte budget simply doesn't run.

### 5.5 Disk accounting — logical bytes + threshold compaction (G-1)
- The budget is measured in **logical entry bytes** (summed, exact, predictable).
- Because `redb` reuses freed pages rather than shrinking the file, a **Compactor** reclaims space so the actual file tracks the budget: when a shard's free-space ratio crosses `COMPACT_THRESHOLD` (e.g., >50% free) **and** the shard is idle, run `redb` compaction off the hot path; also opportunistically after large evictions. This closes the "budget that doesn't bound disk" gap.

### 5.6 What collapses
| Old mechanism | Replaced by |
|---|---|
| Shard age cleanup (A1) | continuous byte-budget eviction |
| Shard size cap 512 MB (A2/E2) | global byte budget (entry-granular, includes active shard) |
| Disk entry cap 20k (A4/E4) | global byte budget |
| Recycle 200k (A7/E7) | deleted |
| Memory cap 4096 (A3/E3) | plain working-set bound |
| `RECENTS_TABLE` + touches (8f) | `last_seq` in the entry |

---

## 6. Stores

### 6.1 Registry store — separate, bounded, honest
- **Separate file** (unchanged location), its own economics (losing an entry = network refetch, not local recompute).
- **Own size limit:** new setting `importLens.registryCacheMaxSizeMB` (default modest, e.g., 32). Enforced by LRU/retention eviction of oldest `updated_at` entries when over budget.
- **Automatic retention that actually runs:** `purge_expired` (30-day `updated_at`) runs on **startup and periodically**, not only on a user button (fixes A5 / X-15).
- **Real `clear()` that sticks (fixes X-14):** empties the map and writes an empty snapshot **bypassing the persist-time union** (the union at `cache.rs:146-153` currently resurrects removed entries). Retention and clear both write authoritatively.
- **Manual-refresh governor (Adjustment 1):**
  - *Single-flight* — concurrent refreshes of the same package share one network call.
  - *Per-package cooldown* — a forced refresh within `MANUAL_REFRESH_COOLDOWN` **coalesces** to the just-fetched value (a re-click is a no-op, not a new request or an error).
  - *Global token bucket* — a bounded manual-refresh rate, stricter than background refresh, honoring `Retry-After`.
  - *Bounded bulk* — "refresh dependency block" drains through the isolated registry worker pool with a **max in-flight** cap (200 deps → controlled trickle, not 200 sockets).

### 6.2 L1 file-size / module-graph / resolver caches
- Remain small in-memory derived caches with existing caps (L1=64, graph=32).
- **All three are cleared by "Clear everything"** — including the resolver cache `SHARED_RESOLVERS`, which no button clears today (fixes X-16).
- L1/graph clears are **no longer gated on `!removed.is_empty()`** (fixes X-21), and any clear that empties bundle caches also bumps generation (see §9, fixes X-17/X-20 asymmetry).
- **L1 aggregate inherits component freshness.** The current-file total is `Stale`/`Unverified` if *any* contributing import is — it carries the strongest (least-fresh) component marker, so the status-bar size can't advertise `Fresh` while a component is being revalidated.

---

## 7. Validity automation (fixes A6 being button-only)

`cache_key_is_orphan` is split by lifecycle:
- **Analyzer-version staleness → reclaimed automatically by the byte budget.** A daemon upgrade mints new keys (`ANALYZER_VERSION` is in the key), so old-analyzer entries go cold and are evicted oldest-first by continuous eviction (§5.3) — automatic, not button-gated, with no dedicated startup scan to maintain (fixes X-18). They're never *served*: a stale-version row fails decode on the off chance its key is hit.
- **Path-missing (uninstalled) → automatic on `NodeModulesChanged`.** The watcher event that already fires on install/uninstall triggers reclaim of entries whose package paths are gone — piggybacked, no project scan (fixes X-19).
- **The user "Purge Orphans" button is removed** as a separate action; its job is now automatic. Deliberate clearing remains available via Manage Cache (§8). Path-missing checks use the **`NotFound`-only** rule from §4.3, so a transient/offline path never triggers deletion.

---

## 8. UI — one entry point, scoped clears, honest reporting

- **One command: `ImportLens: Manage Cache`** opens a quick-pick that first shows **read-only status**, then scoped clear actions:
  - Clear **current project**
  - Clear **all projects** (computed bundle caches)
  - Clear **registry metadata** (npm hints)
  - Clear **everything**
- **"Clear everything" is truly everything:** all shards + rollups + registry (union-bypassing) + resolvers + L1 + graph, and bumps generation. Removes the "which button?" overlap (Run Cleanup Now / Purge Orphans are gone — both are automatic now).
- **Observability (fixes X-24):** the status view shows total size vs budget, per-project size/entry-count/last-used, registry size, and headroom; clear/reclaim operations report **accurate entry-level counts**.
- **Honest messaging (fixes X-22/X-23):** one consistent "Clear all" phrasing; the success toast states exactly what was cleared. The three formerly-hidden actions are gone (folded into automatic maintenance), so the command surface matches the docs.

---

## 9. Data model & IPC changes

- **`CacheIdentityV4`** (§4.7) — key without in-key fingerprints.
- **`EntryRecord`** — adds `content_fingerprints: Vec<FileFingerprint>`, `generation: u64` (captured-at-read), `last_seq: u64`, `bytes: u64`; drops reliance on `RECENTS_TABLE`.
- **`ResultFreshness`** (§4.5) — on the size-result payload over IPC.
- **Shard summary** — `total_bytes`, `entry_count`, `oldest_seq` persisted per shard; new by-`last_seq` index.
- **Protocol:** `CacheRemoveScope` gains explicit `Registry`; `Orphans` scope retired from the UI (logic moved to automatic). Status response carries the observability fields (§8).

---

## 10. Concurrency & ownership (G-4)

- **`BudgetCoordinator` is the single writer** for byte accounting + eviction. The four inserter paths (interactive analysis, prefetcher, SWR revalidation, WorkspaceReport) route inserts/rollup updates through it; it owns the batched flush + eviction. This composes with `redb`'s single-writer-per-DB reality.
- Synchronous IPC handlers remain serialized by the request loop; only the two detached paths (WorkspaceReport, Prefetcher) overlap invalidation, and §4.2 makes their late inserts safe.
- The SWR in-flight set and the registry single-flight set are the two dedupe points; both are concurrent maps keyed by cache key / package.
- **Accounting handles replace, not just insert/remove.** An SWR refresh (or any re-insert) whose value size differs updates `total_bytes` by the delta — `record()` is insert-**or-replace** — so the budget never drifts when a recomputed size changes.

---

## 11. Migration & versioning (G-2)

- **One general rule, zero per-version migration code — ever.** Every store stamps a `schema_version`. On open, if the stored version ≠ the current expected version, **wipe that store and recreate it empty**, regardless of which versions are involved. The v3→v4 bump is just the first instance; every future bump is handled the same cheap way. No "detect v3 / `RECENTS_TABLE` present" logic, no transforms — just *version differs → wipe*.
- **Scoped per store, so a bump doesn't over-wipe.** The bundle-cache schema version covers shard format + key identity (v4); the registry carries its **own** version (JSON, rarely changes). A bundle-cache bump therefore never nukes the network-fetched registry, and vice-versa.
- One **cold-cache moment** for the wiped store per bump is the accepted price; prewarm repopulates quickly. (Note: the analyzer version lives *inside* the bundle key, not the schema version, so an ordinary daemon release does **not** trigger a wipe — old-analyzer entries just go cold and are reclaimed by the byte budget; see §7.)

---

## 12. Robustness & error handling

- **Transient-vs-gone** everywhere paths are checked: only `ErrorKind::NotFound` counts as gone; every other error is `Unknown → keep` (§4.3). Applies to the freshness probe, path-missing reclaim, and shard `project_root` checks (fixes X-3/X-4).
- **`recreate_database` (X-5):** never delete a DB on a *transient* open failure; retry with backoff and only recreate on a genuine corruption signal. A concurrent double-open must not wipe a live shard.
- **Clear/remove bumps generation (X-17)** so an in-flight analysis can't silently re-populate a just-cleared store as fresh.
- **Eviction is idempotent and crash-safe:** rollups are rebuildable from shards; a crash mid-evict leaves shards authoritative and heals on next startup.

---

## 13. Settings

| Setting | Change |
|---|---|
| `importLens.cacheMaxSizeMB` (512) | **Kept**, semantics clarified: the **global total** disk-byte budget. |
| `importLens.cacheMaxAgeDays` (30) | **Deprecated** (age cleanup removed). Ignored gracefully; marked deprecated in `package.json`; no error if present. |
| `importLens.registryCacheMaxSizeMB` (new, ~32) | Registry store size limit. |
| `importLens.serveStale` | **Not added now** — reserved; the data-layer freshness flag makes it a future presentation toggle. |

---

## 14. Component boundaries (each unit: purpose → interface → depends on)

| Unit | Purpose | Interface | Depends on |
|---|---|---|---|
| `FileFingerprint` | content-hash + pre-filter | `capture(bytes,meta)`, `is_current(path)->Fresh/Stale/Gone/Unknown` | fs, hasher |
| `FreshnessProbe` | tri-state validity | `check(entry)->Freshness` (non-evicting) | `FileFingerprint`, generation |
| `SwrRevalidator` | dedup + bg recompute | `revalidate(key)` | analyze pipeline, in-flight set |
| `RecencyClock` | monotonic recency | `next()->u64` | — |
| `ShardIndex` | per-shard seq index + summary | `insert/remove/oldest(n)` (evict) / `newest(n)` (prewarm) / `summary()` | redb |
| `BudgetCoordinator` | single-writer budget + eviction | `record(entry)`, `on_access(k,promote)`, `evict_if_needed()`, `remove(scope)`, `total_bytes()` | `ShardIndex`, registry of shards |
| `Compactor` | reclaim redb free pages | `maybe_compact(shard)` | redb, `ShardIndex.summary` |
| `RegistryStore` | bounded npm-hint store | `get/put/clear/purge_expired/refresh(governed)` | fs, net client, rate limiter |
| `ValidityMaintenance` | auto reclaim | `sweep_version()` (startup), `reclaim_missing(pkg)` (event) | shards, generation |
| `CacheManager` (ext) | one entry, scoped clears, status | quick-pick → IPC scope | IPC protocol |

---

## 15. Testing strategy

**Correctness (Layer 1)**
- TOCTOU: modify a dep file *during* analysis → entry is not served as fresh (§4.2).
- D4 race: bump generation mid-analysis → late insert carries `G_start` → next read re-validates → stale/evict, never permanent.
- Tri-state: dep changed → Stale+SWR; dep `NotFound` → Gone+recompute; dep locked (simulated transient) → Unknown+keep (no eviction).
- Content hash: identical-content reinstall (new mtime, same bytes) → **hit**; same-length same-ms edit → **detected** (hash differs).
- D3: directly-edited first-party dep bypasses fast path → refresh; SWR shows last value, not loading.
- Monotonic clock: backward wall-clock jump does not extend a window.

**Capacity (Layer 2)**
- Global eviction across shards down to low-water; active shard *is* subject to the budget (unlike A2).
- Per-project floor: large project can't evict a small project's floored entries.
- Scan resistance: a WorkspaceReport does not promote recency / flood the working set.
- No dangling recency possible (structural — assert no orphan index rows after remove/evict/invalidate).
- Compaction: after mass eviction, file size tracks logical bytes once compacted.
- Rollup rebuild: kill+restart mid-evict → startup reconciles, totals correct.

**Stores / UX**
- Registry `clear()` sticks (no resurrect on next persist); retention runs on startup; own size limit enforced.
- Manual-refresh governor: burst of same-package refreshes → one network call; bulk block → bounded in-flight; cooldown coalesces.
- "Clear everything" empties shards + registry + resolvers + L1 + graph and bumps generation; status/toast counts are accurate.

**Migration / robustness**
- Old-format shard on open → discarded cleanly, no crash, prewarm repopulates.
- Transient stat error never deletes an entry or a shard.

---

## 16. Implementation sequencing (phases — detailed plan follows via writing-plans)

1. **Correctness core.** `FileFingerprint` (content hash), atomic capture (`generation` + hashes before read), tri-state `FreshnessProbe`, monotonic clocks, transient-vs-gone rule. *(Kills X-1, D3, D4, X-2, X-4, X-6; makes value-side validation exact.)*
2. **Identity v4 + L1 signature.** Drop in-key fingerprints; fold document fingerprint into L1. *(D2; depends on Phase 1.)*
3. **SWR.** `get_with_freshness`, in-flight dedupe, `ResultFreshness` over IPC. *(D1 + Adjustment 2.)*
4. **Recency + capacity.** `RecencyClock`, entry `last_seq`, per-shard seq index + rollup, `BudgetCoordinator`, evictor with hysteresis + floor, scan resistance; delete `RECENTS_TABLE`/touches/recycle/old caps. *(8f, X-8..X-13, A1/A2/A3/A4/A7.)*
5. **Disk accounting.** Compactor + threshold policy. *(G-1.)*
6. **Registry store.** Separate bound + setting, union-bypassing `clear()`, auto-retention, manual-refresh governor. *(A5, X-14, X-15, Adjustment 1.)*
7. **Validity automation + UI.** Event-driven path-missing reclaim (analyzer-version churn rides the byte budget, §7), one Manage-Cache entry with scoped clears + observability + honest toasts; resolver clear; generation bump on clear. *(A6 automation, X-16..X-24.)*
8. **Migration.** Schema-version detection + clean discard. *(G-2.)*

Each phase is independently testable; correctness (1–3) lands before capacity (4–5) so we never build eviction on an unsound validity base.

---

## 17. Issue coverage matrix (nothing dropped)

| Source finding | Resolution (section) |
|---|---|
| A1 age / A2 size / A4 disk-count / A7 recycle | §5.3/§5.6 single byte budget |
| A3 memory cap | §5.4 working-set bound |
| A5 registry retention button-only | §6.1 auto-retention |
| A6 orphan purge button-only | §7 automatic (startup + event) |
| 8a Clear-All not all | §6.2/§8 clears registry+resolvers+L1+graph |
| 8f recents unbounded | §5.1 recency in entry |
| D1 no SWR | §4.5 |
| D2 in-key fingerprints | §4.7 v4 |
| D3 30s TTL first-party stale | §4.4 fast-path bypass |
| D4 concurrent re-insert (High, permanent) | §4.2 captured-at-read generation + content hash |
| X-1 TOCTOU | §4.2 |
| X-2 prefetcher race | §4.2 (same capture) |
| X-3/X-4 transient→destructive | §4.3 NotFound-only |
| X-5 recreate_database wipe | §12 |
| X-6 wall-clock TTL | §4.6 |
| X-7 mtime+len blind spot | §4.1 content hash |
| X-8 disk-cap under-eviction / X-9 recents leak / X-10 sort hot path / X-12 tie / X-13 preload waste | §5.1 (side tables removed) |
| X-11 memory-cap herd | §5.4/§10 single owner |
| X-14 registry unbounded + resurrect | §6.1 |
| X-15 purge_expired dead | §6.1 auto-run |
| X-16 resolver cache uncleaned | §6.2 |
| X-17 no gen bump on clear | §6.2/§12 |
| X-18 version reclaim button-gated | §7 continuous byte-budget eviction (no sweep) |
| X-19 uninstalled never self-heal | §7 event reclaim |
| X-20 cleanup vs clear L1 asymmetry | §6.2 (cleanup removed; clear consistent) |
| X-21 clear gated on non-empty | §6.2 |
| X-22 command-surface mismatch | §8 |
| X-23 naming / toast overclaim | §8 |
| X-24 purge counts invisible | §8 observability |
| G-1 redb free pages | §5.5 compaction |
| G-2 migration | §11 |
| G-3 freshness vs lookup cost | §4.4 fast/slow path |
| G-4 concurrency ownership | §10 |

---

## 18. Explicitly out of scope / kept simple

- No stale-badge UI or `serveStale` setting yet (data flag only).
- No third-party cache library; no per-entry cross-shard *persisted* global ledger (rollups instead).
- SWR kept minimal: one in-flight set, no priority tiers.
- Per-project floor is a small fixed count, not a configurable policy.
- Analysis pipeline internals unchanged upstream of the fingerprint-capture point.
- v3→v4 discards existing cache contents (no in-place migration).
- **Bundle-impact history is user data, not a cache** — it isn't LRU-evicted here. Out of scope for this redesign, but flagged for a small adjacent follow-up: give it its own **retention cap** (max records) so it can't grow unbounded. (The audit swept for other unbounded stores and found only registry + resolvers — both handled here — so history is the one *non-cache* store still worth bounding, separately.)
