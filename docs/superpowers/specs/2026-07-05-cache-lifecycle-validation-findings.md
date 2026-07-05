# Cache Lifecycle — Validation Findings

> **Companion to** `2026-07-05-cache-lifecycle-redesign-brief.md`. The brief was written pre-validation. This doc records the result of a code-level audit (four parallel readers over the whole cache surface) that (a) validated every claim in the brief against `main`, (b) corrected three of them, and (c) surfaced 20+ additional issues with `file:line` evidence. It is the evidence base for the redesign spec.
>
> Verdict legend: **CONFIRMED** / **PARTIAL** / **REFUTED**. Severity: Critical / High / Medium / Low.

---

## 1. Verdict on the brief's claims

| Claim | Verdict | Severity | Note |
|---|---|---|---|
| A1 — shard age cleanup 30d, whole-shard, startup/explicit only | CONFIRMED | Low (by design) | `project.rs:162-181`, default `service.rs:62` |
| A2/E2 — 512 MB shard cap, whole-shard, active shard never trimmed | CONFIRMED | Medium | `project.rs:185-204`; active shard's `last_used` always freshest → sorts last |
| A3/E3 — memory cap 4096/shard, O(n) min-scan per insert | CONFIRMED | Low | `memory.rs:27,202-214` |
| A4/E4 — disk cap 20k/shard, count-based, recents not trimmed | CONFIRMED | Medium | `disk.rs:32,641,653-660` |
| A5 — registry retention 30d, only via Purge Orphans button | CONFIRMED | Medium | `constants.rs:14`, `cache.rs:97`, sole caller `service.rs:1112` |
| A6 (i–v) — orphan purge scope | CONFIRMED (all sub-parts) | Medium | see §4; trigger = button only |
| A7/E7 — recycle at 200k total entries, redundant | CONFIRMED | Low | `lifecycle.rs:9`; operates on **in-memory** total (4096 × loaded shards) → needs ~49 concurrent projects → effectively dead |
| 8a — "Clear All" does not clear registry; no registry `clear()` exists | CONFIRMED | **High** | `service.rs:1110`; registry has only `purge_expired`, no clear/truncate |
| 8f — `recents` table unbounded; dangling rows; trim never implemented | CONFIRMED | Medium | `disk.rs:683`; C3/Task-7.2 checkbox left unchecked |
| D1 — no SWR; `get()` deletes on mismatch, returns None | CONFIRMED | Low (feature gap) | `memory.rs:122-127`; does **not** distinguish changed/gone/transient |
| D2 — in-key fingerprints orphan on reinstall; value-side re-validation suffices | CONFIRMED (mechanism); PARTIAL on "every reinstall / biggest source" | Medium | see §3 + §6 |
| D3 — 30s TTL serves stale for directly-edited first-party deps | CONFIRMED | Medium | `memory.rs:36,119-146`; 3 gen-bump sites, all node_modules |
| D4 — concurrent re-insert stamps stale entry | **CONFIRMED, but "self-heals after TTL" REFUTED** | **High** (brief said Low) | see §2 |

---

## 2. Corrections to the brief (things the brief got wrong)

- **C-1 — D4 is High, not "Low, self-heals."** `analyze_and_cache` computes the result *then* captures fingerprints *after* (`service.rs:1355` → `:1358`). A stale result is paired with **fresh** fingerprints, so the slow-path re-verify (`memory.rs:123`) sees them current and **re-stamps as fresh instead of evicting**. The stale size persists **permanently** until the dep changes again or the package is explicitly invalidated — not for 30s. This is the single most severe finding.
- **C-2 — "None of the bounds run continuously" overstates.** Only the *byte-size* budget (`cleanup()`, 512 MB) is startup/explicit-only. The **count** caps run continuously: memory cap every insert (`memory.rs:160/194`), disk cap every 64-insert flush (`disk.rs:151-158`), recycle every 60s + per message. The real gap is narrower and sharper: *the only bytes-aware bound is the one that rarely runs, and it structurally never touches the active project's shard.*
- **C-3 — "Every reinstall orphans entries / biggest source" overstates frequency.** A normal reinstall fires `NodeModulesChanged` → `invalidate_packages` by `package_name` (`server.rs:638`, `disk.rs:323`), which sweeps the old key regardless of fingerprints. In-key fingerprints only create an *un-reclaimable* orphan when the **watcher misses** (watcher-excluded folders — the code calls this case out at `memory.rs:32-34`). The redundancy thesis still holds in both cases, so the D2 conclusion (drop in-key fingerprints) is sound — just not for the stated reason.

---

## 3. The linchpin (most important cross-cutting insight)

**Fingerprint-capture *timing* is the root of the correctness problems, and it gates the biggest simplification.**

- It is a standalone **TOCTOU** bug (§5, X-1): because fingerprints are captured *after* the result is computed, any dependency module edited *during* an analysis window yields old-result + new-fingerprints under a live key that `fingerprints_are_current` will never flag — **permanent staleness with no invalidation involved at all.** D4 is one trigger of this; the TOCTOU is the general case.
- It **blocks D2.** The brief's highest-leverage cleanup — drop in-key fingerprints and lean on value-side `dependency_fingerprints` re-validation — increases reliance on exactly the path that has the TOCTOU hole. So the safe sequence is: **fix capture timing first, then D2 becomes safe.**
- The fix is already proven in-repo: **L1 folds `cache_generation()` into its signature** (a lookup input) and is therefore immune to D4 (`file_size_cache.rs:164`). **L2 stamps generation as a claim** (`memory.rs:181`) and is vulnerable. The structural fix is to make generation/fingerprints part of the freshness *derivation*, or to snapshot fingerprints *before* content is read for analysis.

**Recommended sequencing:** fix capture-timing/generation model → drop in-key fingerprints (D2) → orphan pressure falls → collapse the overlapping capacity machinery into one size-budget LRU. Capacity is what the brief focused on; **correctness is the larger latent risk and should lead.**

---

## 4. Orphan purge — sub-part verdicts (A6)

All CONFIRMED. `cache_key_is_orphan` (`key.rs:96-114`) is deliberately path-existence based, **not** fingerprint based (comment `key.rs:90-95`) — so fingerprint-superseded entries are invisible to it. It conflates two orphan classes that want different lifecycles:
- **analyzer-version staleness** (`key.rs:100-102`) — a *correctness* condition, cheap, predictable at every release → belongs in **automatic startup cleanup**.
- **path-missing** (`key.rs:103-114`, `project.rs:256-261`) — a filesystem GC condition → belongs on the **`NodeModulesChanged`** path (which already fires on install/uninstall).

Splitting them lets both go automatic and demotes the user button to a manual backstop.

---

## 5. New issues (not in the brief), by theme

### Correctness (highest priority)
- **X-1 — Fingerprint-after-compute TOCTOU → permanent staleness, no invalidation required.** `service.rs:1355` (compute) then `:1358` (stat). Generalizes D4. **High.**
- **X-2 — Prefetcher is a second concurrent re-insert path.** Prewarm on detached threads → `analyze_and_cache`; cooperative cancel can't stop a job already inside; `should_store` can pass just before the bump/cancel. `prefetch.rs:83,104,247,298`. **High/Medium.**
- **X-3 — `Path::exists()`-means-gone false positives at four levels, worst whole-shard.** `exists()` is false on *any* metadata error (locked/mid-write/offline drive), not just NotFound. Purge Orphans while a network-drive project is disconnected `remove_dir_all`s its **entire shard**. `project.rs:256-257`, `key.rs:103-114`, `file_size_cache.rs:110-111`, `graph.rs:290-291`. **Medium.** *(Convergent: agents 2 and 4 both flagged the stat-error family.)*
- **X-4 — Transient stat error evicts valid entries.** `file_fingerprint` returns None on any `fs::metadata` failure → `fingerprints_are_current` false → `get()` removes both memory + disk copies. `key.rs:154-169`, `memory.rs:123-126`, `disk.rs:115-119`. **Low-Medium.**
- **X-5 — `recreate_database` deletes the cache DB on any open failure**, including transient lock contention during a concurrent unloaded-shard open. `disk.rs:485-518`, `project.rs:275,318`. Windows-mitigated (sharing violation blocks the unlink); diverges on Unix. **Low-Medium.**
- **X-6 — Wall-clock (not monotonic) re-verify TTL.** `now.saturating_sub(verified_at)` with `SystemTime::now()` (`time.rs:11`, `memory.rs:120`): a backward clock jump makes every entry read "fresh" and skips re-stat until the clock catches up. Registry deliberately uses `Instant` (`registry/service.rs:19`); the cache does not. **Medium.**
- **X-7 — Fingerprint is `mtime-millis + len`, no content hash.** Same-length edit within one millisecond (or coarse-mtime FS) is invisible. `key.rs:18-22`. **Low.**

### Capacity / eviction bugs
- **X-8 — Disk cap can be left violated (under-eviction).** `excess` is derived from `CACHE_TABLE.len()` but victims are chosen from `RECENTS_TABLE` (a superset with dangling rows); evicting a dangling key is a no-op, so the table stays over cap until later flushes. `disk.rs:637-661`. **Medium.**
- **X-9 — Dangling `recents` leak permanently on disk.** `remove()` / `purge_orphan_entries` never drain `pending_touches` (unlike `insert`/`invalidate_packages`), and dangling rows are invisible to every reaping path except full clear. They survive daemon restarts (per-project lifetime). `disk.rs:248-266,372-415`. **Medium.**
- **X-10 — O(R log R) recents scan+sort on every over-cap flush** — the true hot path (worse than the O(n) memory scan), inflated by dangling rows. `disk.rs:643-652`. **Medium (perf).**
- **X-11 — `enforce_memory_cap` thundering herd + weakly-consistent snapshot.** k concurrent inserts each run the O(n) `min_by_key` on Relaxed loads; can evict a just-hydrated entry. Correct but wasteful. `memory.rs:202-214`. **Low.**
- **X-12 — LRU tie non-determinism.** Recency is wall-clock ms; same-ms ties evict in arbitrary order, can drop a hotter entry. A monotonic sequence counter fixes it. **Low.**
- **X-13 — Dangling recents waste startup preload/prewarm slots.** `recent_keys` returns dangling keys; `load_recent` then drops them → fewer real entries prewarmed. `disk.rs:301-313`. **Low-Medium.**

### Unbounded / uncleaned stores
- **X-14 — Registry metadata is the *only truly unbounded* cache store.** Whole JSON loaded to memory; every persist re-unions the on-disk view (`cache.rs:146-153`) so a naive `clear()` would **resurrect** entries; the only bound is 30d retention that fires only on the hidden button. No size/count cap (contrast L1=64, graph=32). **High.**
- **X-15 — Registry `purge_expired` is effectively dead on all automatic paths.** Startup cleanup never touches the registry; there is no timer/load/persist trigger. `service.rs:1030-1034`, sole caller `service.rs:1112`. **Medium.**
- **X-16 — "Clear All" also skips the resolver cache** (`SHARED_RESOLVERS`), cleared only by `invalidate_*` which no button calls. So Clear All leaves *two* stores behind (registry + resolvers). `resolver.rs:551`, `service.rs:1153-1159`. **Medium.**

### Missing-cleanup / concurrency / UX
- **X-17 — `remove_cache` never bumps generation** → a concurrent analysis can re-create a shard / re-populate graph+L1 immediately after Clear All. `server.rs:478`. **Low.** *(Same missing-bump theme as X-1/D4.)*
- **X-18 — Stale-analyzer-version reclaim is button-gated** though a release orphans potentially the whole cache at once; it's *validity*, not GC → should be automatic at startup. **Low-Medium.**
- **X-19 — Uninstalled-package entries never self-heal** (old key never re-accessed); could piggyback on the `NodeModulesChanged` that already fires. **Low-Medium.**
- **X-20 — Run Cleanup Now doesn't clear L1** (asymmetry with Clear All) → stale status-bar aggregate after a cleanup. `service.rs:1033` vs `:1134-1137`. **Low.**
- **X-21 — L1/graph clear gated on `!removed.is_empty()`** → a no-op Clear All skips the in-memory clears. `service.rs:1133`. **Low.**
- **X-22 — Command-surface / README mismatch.** 3 of 5 Manage-Cache actions (Run Cleanup, Purge Orphans, Inspect) have no palette command and are undocumented; the sole registry-touching action is the most hidden. **Medium.**
- **X-23 — Inconsistent "Clear All" naming** (four phrasings, all overclaim vs shards-only) + **misleading success toast** ("removed all" while registry+resolvers survive). **Low-Medium.**
- **X-24 — Entry-level purge count invisible to the user** (return values discarded) → "0 removed" after dropping many entries. `project.rs:272,280`. **Low.**

---

## 6. Refuted worries / do-NOT-fix (avoid over-engineering)

- **v3 identity is collision-free and order-stable.** The key is the *lossless* hex(msgpack) of the identity, not a digest (`key.rs:149-152`); `named_exports` sorted+deduped; serde preserves order. No hash-collision surface. The 64-bit FNV shard id (`project.rs:601`) is a theoretical collision only — ignore.
- **Validity vs capacity are *already* mostly separate.** Validity = get-path (`fingerprints_are_current`, `analyzer_version`) + orphan purge; capacity = `write_pending_inserts` + `enforce_memory_cap`. Only `cleanup()` mixes age+size, and both are recency GC — defensible. Don't do a grand validity/capacity re-architecture; do the targeted fixes above.
- **D2 is safe to do** once capture-timing is fixed: in-key fingerprints add **zero** correctness value over the value-side re-validation (which already covers package.json + entry + the *full module graph*, `service.rs:1546-1557`). The one coupling to untangle is **L1's signature**, which currently borrows mtime sensitivity from the key (`file_size_cache.rs:146`) and would need to fold a document fingerprint in independently.

---

## 7. What this means for the redesign

The brief's agreed direction — **a single total disk-size budget with LRU eviction, replacing the overlapping bounds** — is validated as correct for the *capacity* layer (it's the only thing that actually caps the active project's bytes; X-8/X-10/X-12 and the whole two-queue recents design collapse away if recency lives *inside* the entry with a monotonic sequence). But the audit shows the redesign must also address a **validity/correctness layer** the brief underweights:

1. **Correctness first:** fix fingerprint-capture timing / make generation a lookup input (kills X-1, D4, X-2); then D2 (drop in-key fingerprints).
2. **One recency source** stored in the entry record (kills 8f, X-8..X-13).
3. **One size-budget LRU** evictor at entry granularity (subsumes A1/A2/A3/A4/A7).
4. **Every store bounded + every store cleared:** registry gets a real `clear()` (union-bypassing) + an automatic bound; resolvers included in Clear All; L1/graph/recents all reaped consistently (X-14..X-24).
5. **Validity goes automatic** (analyzer-version at startup, path-missing on `NodeModulesChanged`); the user button becomes a backstop.
6. **Collapse buttons 5 → 3** (Clear All = truly everything; Clear Current; Manage/Inspect); honest toasts; documented surface.
7. **Robustness:** treat transient stat errors as "unknown, keep" not "gone, evict" (X-3/X-4/X-5); monotonic clock for TTLs (X-6).
