# Known Issues

The full tracker of known issues on this project: release blockers that must be fixed before shipping,
work deferred for later, and behaviours we have accepted and are content to leave. Everything found and
not yet resolved is recorded here so nothing is lost. Entries are ordered by priority, highest first.

## How to use this file

Record an issue here when you decide how to treat it, not merely when you find it. A **Blocker** must be
fixed before release. A **Deferred** item is worth doing and should become a task. An **Accepted** item we
are content to leave. Every entry states what actually happens and why it is treated the way it is. An entry
with no failure scenario is a rumour, and a rumour in a tracker is worse than nothing.

### The bar that decides a blocker

> Fix it before release only if it (a) shows the user a WRONG NUMBER, or (b) can WEDGE the system or lose
> data. Everything else is recorded here and queued.

A real finding is not the same as a blocking one. A chain of four rounds on a conservative edge case once ran
while eleven plan tasks sat untouched. "Real" was never the right bar.

### Status values

| Status | Meaning |
| --- | --- |
| **Blocker** | Must be fixed before release. Shows a wrong number, or can wedge or lose data. |
| **Deferred** | Worth doing, not now. Should become a task. |
| **Accepted** | We know, we are not fixing it, and we are content. Revisit only if the blast radius changes. |
| **Watch** | Not a defect today. Becomes one if some condition changes. |
| **Unverified** | Shipped without the review we normally require. Not known to be wrong. |
| **Resolved** | Fixed. Kept for history. |

---

# Priority 0: release blockers

Nothing is open. B1 (the all-inline-`type` over-count), B3 (native-binary mislabeling), and B2 (non-JS asset
counting) have all landed and moved to Resolved below. B1 and B3 shipped together under a single
`ANALYZER_REVISION` bump to `rolldown-1.1.x+4`; B2 followed in its own pull request and moved it to
`rolldown-1.1.x+5`.

---

# Priority 1: deferred, worth doing

Real work, queued rather than abandoned. None of these is a wrong number or a wedge today, so none blocks the
release, but each is worth turning into a task.

### D6: "Unavailable" is one label for several causes, and one unbundleable leaf discards the whole package
**Status: Deferred** · The most visible gap in real projects · Fix universally, never per-package

**What the user sees.** In a real `package.json`, a growing fraction of dependencies render **unavailable**,
and not only native CLIs. The bigger the project, the more of them, which reads as "the build was too big." It
is not a size problem.

**"Unavailable" collapses distinct causes into one word.** Measured against esbuild (Import Lens's own
accuracy oracle) on the installed packages, every failure is fast (4 to 300 ms, never a timeout):

| Package | Real cause (confirmed 2026-07-16) | Class |
| --- | --- | --- |
| `@vscode/vsce` | imports `keytar.node` (a compiled native addon) | native leaf |
| `ovsx` | `keytar.node` plus `@node-rs/crc32`'s `.node` | native leaf |
| `@biomejs/biome` | no importable entry (`bin` only); real tool is a native binary. Now handled by **B3** | native binary (B3) |
| `jest` | `jest-pnp-resolver` does `require('jest-resolve/build/defaultResolver')`, unresolvable, so `[resolve]` fails the whole build | unresolvable leaf |
| `eslint-plugin-autofix` | does `require('eslint/lib/built-in-rules-index')` (eslint's non-exported internals), unresolvable, so `[resolve]` fails the whole build | unresolvable leaf |
| `@next/font` | still unconfirmed (no diagnostic captured yet) | needs the daemon's stage |

Confirmed 2026-07-16: `jest` and `eslint-plugin-autofix` are not "pure JS that should measure and does not."
Each fails because one transitive `require` targets a deep internal subpath of another package that the
resolver cannot resolve, which is the "one leaf poisons the whole build" case below, not a hidden measurement
bug. `@biomejs/biome` is native-binary-backed and moves to the B3 blocker.

**The universal defect: one leaf poisons the whole number.** A single unbundleable edge (a `.node` addon, a
dynamic `require`, an unresolvable specifier) anywhere in the graph fails the ENTIRE package build, so a 2 MB
JS graph with one native leaf reports nothing instead of "at least 2 MB, excluding a native addon." Import
Lens ALREADY does the right thing for one class: it externalizes an unresolvable bare import (`tsdown` measured
at 134.7 kB where esbuild refused on `@tsdown/css`). The gap is that this leniency does not extend to `.node`
files or unfollowable dynamic requires.

**The universal fix (never per-package).** At the engine/resolver boundary, treat every unbundleable leaf as
an external boundary rather than a hard failure: measure the JS graph that did bundle as a **floor**, and
disclose the uncounted leaf exactly as non-JS asset bytes are disclosed today (B2). Same shape as D2 (a floor beats a blank),
but triggered by a build error, not only a graph-limit breach. Pair it with a labeled reason in the UI
("native addon (keytar)", "no importable entry", "unresolved: X") so the badge names the truth instead of a
blanket "unavailable." Do this at the boundary, so it covers every package by construction; do NOT special-case
`keytar`, `jest`, or any named dependency.

**Why it is not a blocker.** For the unresolvable-leaf case, "unavailable" is honest today: no wrong number,
cannot wedge (it is the anti-fabricator working). This is a coverage and UX upgrade, not a correctness fix. The
"pure-JS package that should measure and does not" suspicion was flushed out on 2026-07-16: `jest` and
`eslint-plugin-autofix` both fail on a genuine unresolvable deep-path `require`, not a measurement bug. The
native-binary packages that used to sit here (for example `@biomejs/biome`) are the correctness blocker B3.

**Prerequisite: get the distribution before designing the fix.** A diagnostic that runs the daemon over a real
project's whole dependency set and buckets each "unavailable" by its actual stage (native-leaf, no-entry,
unresolved, dynamic-require, graph-limit, timeout, parse) sizes the fix and surfaces any genuine bug. Build
that first.

### D7: A stylesheet its own package declares droppable is counted anyway
**Status: Deferred** · A wrong number on a package shape measured to be absent from the real ecosystem · Found by the B2 adversarial review

A bundler DROPS a bare `import "./styles.css"` from a package declaring `"sideEffects": false`, so that CSS
never ships. Import Lens counts it anyway: the plugin banks an asset in the `load` hook, and rolldown only
decides side effects afterwards, so the asset is recorded before the decision that discards it. For such a
package the reported Import Cost includes bytes the user's bundle will not carry.

**Why it is not a blocker: the shape was measured, not assumed.** Zero of the 503 packages in this repo's store
bare-import CSS from their entry (118 declare `sideEffects: false`). Zero of 44 real CSS-shipping packages
surveyed on npm have both halves: only `react-select` and `@fullcalendar/core` declare `sideEffects: false`, and
neither imports CSS. Every real CSS shipper is in the correct bucket, declaring `["**/*.css"]` or nothing at
all. The shape is a self-inflicted packaging bug that silently drops the package's own styles in webpack,
rollup, and vite, which is exactly why maintainers do not ship it.

**Do NOT fix it by filtering the collected assets against what the build retained.** That inverts into a far
worse under-count: the `Empty` stub gives a stylesheet no statements, so rolldown treats it as side-effect-free
and drops it even when the package declares nothing, which is the common and correct case. Filtering by
retention would zero out the CSS for `@uiw/react-md-editor` and undo B2 entirely. The honest fix asks the
DECLARATION rather than the build: `SideEffectsMode::False` already identifies the case at the single-import
boundary, and `resolver` already owns rolldown's own glob matcher for per-asset matching. The File Cost path
needs per-asset package attribution first, which is the real work.

### D8: One stylesheet Lightning CSS cannot parse falls back alone, but a cyclic one undercounts
**Status: Accepted** · Never below the pre-B2 floor · Found by the B2 adversarial review

Lightning CSS parses plain CSS. A published package that imports a preprocessor source (`.scss`, `.less`) or a
stylesheet with a bare `@import "pkg/base.css"` cannot be bundled, so that sheet falls back to raw-byte
disclosure. That is the ADR-0006 fallback working: it lands exactly on the pre-B2 behaviour, never below it.
Originally one such sheet sank every stylesheet in the set; a failed set now retries per sheet, so only the
offender falls back and the rest stay counted. In that degraded mode two sheets sharing an `@import` are no
longer deduped against each other, which over-counts the shared part, a smaller and rarer error than dropping
them all.

A stylesheet caught in an `@import` cycle keeps its `@import`ed rules but loses its own, which undercounts that
one sheet. Cycles are silent in browsers and in every real bundler, so a package can ship one unknowingly. It no
longer threatens the daemon (that wedge is fixed and pinned by a regression test); it is now only an accuracy
edge on broken input, and still strictly better than before B2, when the package contributed zero CSS either
way.

The `asset-counting-plan.md` claim that the provider falls back to `oxc_resolver` for a bare `@import` describes
work that was not built; the plan is corrected rather than the gap papered over.

### D10: Every reported brotli size is high, because the daemon compresses at quality 4
**Status: Deferred** · A systematic over-report on every package · Surfaced by the B2 oracle re-baseline

The daemon compresses brotli at **quality 4** (`pipeline::compress`), while the web serves **quality 11**. So
every brotli figure Import Lens shows is larger than what a CDN actually delivers, by 2.6 to 15% across the
accuracy benchmarks and around 25% on highly-compressible CSS. It is not an asset-counting artifact and it
predates B2 by a long way: the accuracy oracle compresses at quality 11, which is why every benchmark has always
read high, and it is the entire reason the CSS benchmark needed its own tolerance rather than the shared gate
being loosened.

Quality 4 is a deliberate speed choice (the compressor runs per keystroke and quality 11 can take seconds on a
large chunk), so this is a real trade, not an oversight. But the number is presented as the brotli size, and it
is not the brotli size anyone ships. The honest options are to compress at 11 off the interactive path (a
background refinement of the number), to name the figure for what it is, or to accept it deliberately. It is
recorded rather than fixed because it touches every number in the product and every baseline that gates them,
which is its own piece of work.

### D9: A stylesheet's own `@import` tree is bounded at 256 files
**Status: Accepted** · A bound where there was none · Found by the B2 adversarial review

A stylesheet's `@import` children are never graph modules, so none of the engine's limits ever applied to them.
Lightning CSS recurses per `@import`, and a deep enough chain overflows the stack, which is NOT catchable: the
process dies rather than the import failing. One tree is therefore bounded to 256 files and 8 MB. Breaching it
is not a wrong number: the set falls back to the per-sheet path, and failing that to raw-byte disclosure.

The file count doubles as the depth bound, because a chain of N files costs N reads and nothing else can see
depth from where the bound is applied. 256 stops the walk roughly three times short of where a release build's
stack gives out, and is far more than any real stylesheet's tree. It cannot simply be raised on the grounds that
a flat set of many sheets carries no stack risk: the bound cannot tell breadth from depth, and giving the walk
its own larger stack does not help either, because Lightning CSS drives the `@import` graph on `rayon` workers
whose stacks it does not own. A set past the bound therefore degrades into the per-sheet path, where sheets
sharing an `@import` are counted once each, which over-counts the shared part and is disclosed.

### D2: An honest lower bound on a failed build
**Status: Deferred** · The intended successor to ADR-0003

Today an unbuildable import reports no size. A graph-limit breach means much of the graph was loaded before we
stopped, so a real floor exists: "at least 4 MB; graph limit exceeded" is strictly better than a blank. The
engine currently discards the partial graph on failure, so this needs plumbing through the engine boundary.

### D4: A file with one unmeasurable import can never cache its total
**Status: Deferred** · A performance cost of an invariant we want

An aggregate missing a contributor's bytes is a **floor**, and a floor is never cached. So a file containing
one permanently-broken import re-runs its (fast-failing) combined build on every size request.

The honest fix is a build memo for the deterministic build failure: a failure caused by the package's bytes is
a fact about those bytes, and the cache is already keyed by their fingerprints. Not caching the total is right;
re-doing the build is waste.

### D3: Marginal cost, a project-level bundle model
**Status: Deferred** · A different product, decided on its own merits

"Adding `zod` here costs nothing, it's already in your bundle." Import Lens measures **imports, not bundles**
([ADR-0004](adr/0004-import-lens-measures-imports-not-bundles.md)) and has no model of what is already in the
bundle. Answering this means building that union model. It is the highest-value idea absent from the design,
and it must be a deliberate decision, not smuggled in as a bug fix.

### G2: A failed (unmeasured) import is counted and badged as a "Conservative estimate"
**Status: Deferred** · Wrong badge or count, never a wrong size · Found in the 2026-07-16 module audit (D9)

`is_conservative_item` (`report/model.rs:92-96`) returns `is_cjs || side_effects || !truly_treeshakeable` and
gates only on `result.is_some()`. `ImportResult::unmeasured` sets `side_effects: true`,
`truly_treeshakeable: false` (`ipc/protocol.rs:337-338`), the honest conservative reading for a build that
produced nothing, so every failed-build row also satisfies the predicate.

**What actually happens.** A workspace report with 1 genuinely-conservative measured import and 2 failed
imports reports `conservative_count = 3`, and each failed row carries a "Conservative estimate" warning stacked
next to its failure message. No byte figure moves: `combined_import_cost_brotli_bytes` sums
`filter_map(row.brotli_bytes)` (`model.rs:74-75`) and an unmeasured row's `brotli_bytes` is `None`
(`model.rs:131`), so the headline, treemap, budget verdict, duplicate-import and shared-module figures all
exclude it (pinned by `an_unmeasured_import_has_no_size_in_the_report_not_a_zero`, `model.rs:509`).

**Why it is not blocking:** it inflates a badge or count on a row the user already sees failed; the S1/R1
"wrong badge, never a wrong size" class, and it cannot wedge (a pure `filter().count()`).
**What would fix it:** gate `is_conservative_item` on a measured size too. A failure is not an estimate but a
different category, so a totally-unmeasured import should not be counted as conservative.

### R2: The legacy entry-field fallback orders `module`, `browser`, `main`, against the resolver's own preference
**Status: Deferred** · Not reproduced as a wrong number · Found in the 2026-07-16 module audit (D2)

`resolve_legacy_fallback` searches the pre-resolved entry in the order `module`, `browser`, `main`
(`resolver.rs:222-232`). For a Client or Component import the resolver itself prefers `browser`, `module`,
`main` (`profile_entry_fields`, `resolver.rs:277`; `main_fields`, `resolver.rs:1046`), so the fallback
contradicts that order.

**What actually happens.** The fallback fires only when oxc's full resolution fails AND the package has no
`exports` map AND no subpath. To pick a different entry than the resolver would, the package must also carry
distinct top-level `browser` and `module` string fields, but when both point at real files, oxc (which also
tries `browser` first) succeeds and the fallback never runs. No concrete package shape was found where oxc
fails yet a usable distinct `browser` string remains, so no served wrong number is demonstrated; the worst
theoretical case is a browser-versus-module entry delta on an exotic malformed package.

**Why it is not fixed now:** not reproducible, so not a wrong number today.
**What would fix it:** reorder the fallback to `browser`, `module`, `main` for parity with the resolver, a
one-line change.

### K2: The project-cache metadata file is written non-atomically
**Status: Deferred** · Self-healing · Off the number-serving path · Found in the 2026-07-16 module audit (D5)

`write_metadata` (`cache/project.rs:1038-1047`) is a plain `fs::write`: no temp-file plus rename, no fsync. A
crash mid-write can leave a truncated or corrupt `metadata.json` for a shard.

**What actually happens, nothing to a served number.** `read_metadata` returns `None` on a corrupt file
(`serde_json::from_str(...).ok()?`), and every consumer drops the shard on `None`: budget eviction,
`invalidate_packages`, orphan sweeps, cache-management listing, and the recency seed. Import numbers are served
through `cache_for_root` then `ImportCache::get` then `DiskCache::get_entry`, which recomputes the shard id
from the root and opens redb without reading metadata (`project.rs:339-404`), rewriting the metadata on that
cold open, so a corrupt metadata file is invisible to the number-serving path and self-heals on next open. The
one theoretical effect (a `NodeModulesChanged` invalidation skipped for the shard) is backstopped by
`check_fingerprints` on the next `get`.

**Why it is not fixed now:** it cannot serve a wrong number, wedge, or lose a durable measurement (the redb
cache is the source of truth; metadata is observability plus invalidation bookkeeping).
**What would fix it:** write to a temp file and `rename` (atomic replace).

**Same pattern, second file (found in the D7b audit).** `record_recycle_timestamp` writes
`importlens-recycles.json` with a plain `fs::write` (`lifecycle.rs:71-87`), read back with `unwrap_or_default()`
on corruption. It gates only idle-recycle detection (after 4 h uptime plus 15 min idle), never a served number;
worst case on a torn write is one extra, already-4h-gated recycle. Same class, same fix (temp plus rename).

### G0: The legacy `performance.rs` smoke suite still claims to gate the NFR numbers, at 8x loose
**Status: Deferred** · Not an active hole, but a second suite that appears to gate what it does not

`daemon/tests/performance.rs` (the pre-existing synthetic-fixture smoke suite) asserts the literal NFR numbers,
`threshold_ms(500)` for a cache miss and `threshold_ms(50)` for a cache hit, with a default multiplier of 6,
and CI's `pnpm test:performance` step sets 8. So it enforces a 4000 ms "cache miss" and a 400 ms "cache hit"
against a hard 50 ms Critical requirement.

**This is not a coverage hole today.** `candidate_performance` now genuinely gates NFR-002 at an absolute,
unscaled 50 ms on every PR, proven by mutation (an 80 ms sleep on the cache-hit path turns it red at 89 ms).
The real gate works.

But it is exactly the shape of the trap that hid the dark gate for months: a suite whose name and thresholds
suggest it enforces a requirement, which in fact enforces something 8x looser. The next person to read it will
believe it.

**Fix:** either stop it naming the NFR numbers (they are its own smoke thresholds, not the requirements), or
delete it now that a real gate exists.

### C7: The engine-permit scheduling model is a repeat source of nondeterminism
**Status: Watch** · Design-health watch. Becomes a redesign task if a THIRD genuine code-level race appears here.

The engine boundary (a fair `Semaphore` of `ENGINE_PERMITS`, with each request handler spawning its own builds)
has produced three distinct nondeterminism issues on this branch:

- A liveness bug (C1): a parked build held a permit forever. Fixed at the design level with `BUILD_TIMEOUT`
  plus a drop-guard.
- A determinism bug (Task 7, `fb7624d`): the failure stage was decided by a parse-versus-resolve race and then
  cached. Fixed by ranking stages in declaration order.
- A test over-assertion (`33411bc`): the streaming test asserted a per-import push reaches the socket before
  the file-size response. `AnalyzeDocument` and `FileSizeDocument` spawn independent tasks that race for the
  two permits, so the trivial per-import build can be starved and the combined build can answer first. No user
  impact (like C4, push and response ordering is not a guarantee), but it flaked CI on a runner with a
  different core count.

**Current assessment: no redesign.** The first two were real defects and were hardened, not patched around.
The third is not a code defect at all: the multiplexing loop is non-blocking and correct; the test claimed a
promise the code never made. The one real cost is that a document's per-import builds and its combined
file-size build duplicate module work while racing for permits, a known perf tradeoff, partly mitigated by
module-level caching, tuned by Task 13's permit count, and invisible to users.

**What flips this to a redesign task:** a THIRD genuine code-level race here (not a test), results that are
wrong or dropped, not merely reordered. At that point the two builds should be coordinated (the combined build
reusing the per-import module builds, which also makes ordering deterministic) rather than left to race. Per
the "redesign at third recurrence" rule, that redesign is the first priority after release blockers and major
fixes.

### E1: `cargo test` fails at full parallelism on the primary dev machine
**Status: Deferred** · Blocks: `pnpm test`, and therefore the pre-push hook

`cargo test` reproducibly fails with `can't find crate for import_lens_daemon` /
`required to be available in rlib format`. It survives `cargo clean` and a fresh target directory. `-j 2`
builds and passes cleanly.

Almost certainly something else touching `target/` concurrently: rust-analyzer running its own `cargo check`,
or antivirus. Not a code defect, but it will bite anyone trying to push.

**Workaround:** `cargo test -j 2`.

---

# Priority 2: accepted, minor or cosmetic

Known, non-blocking, and low value to fix. Each is a wrong badge, a presentation detail, or a graceful
degradation, never a wrong size and never a wedge.

### R1: The "Conservative estimate" warning is path-dependent (interactive versus prefetch)
**Status: Accepted** · Wrong badge, never a wrong size · Found in the 2026-07-16 module audit (D2)

The resolver computes `is_cjs` from oxc's real resolution on the interactive path (`resolver.rs:128`) but
hard-codes it to `false` on the prefetch-refill path (`resolved_from_cache_identity`, `resolver.rs:153`,
reached from `prefetch.rs`). `CacheIdentity` carries no `is_cjs` (`cache/key.rs`), so the two paths share one
cache key. The value flows only into `result.is_cjs`, whose single consumer is the "Conservative estimate"
warning (`report/model.rs:95`: `is_cjs || side_effects || !truly_treeshakeable`).

**What actually happens.** For an extensionless CommonJS entry, the same package can show the warning when
first measured on the interactive path and hide it when the row was populated by prefetch (or the reverse). The
measured bytes are identical either way: `is_cjs` reaches no build input (both `minify_source` calls hard-code
`false` at `analyze.rs:367,453`, and it is not a `BundleRequest` field). Only the warning flips.

**Why it is not fixed:** a badge-consistency issue with no size impact (the S1/K1 class).
**What would fix it:** resolve the entry's format on the prefetch path instead of passing `false`, or fold
`is_cjs` into `CacheIdentity` so the two paths cannot share a row.

### S1: An entry outside its own package root gets a side-effectful badge it may not deserve
**Status: Accepted** · Wrong badge, never a wrong size · Pre-existing

`normalized_side_effect_path` derives the package-relative path by stripping the canonicalized package root
from the canonicalized entry. If the strip fails, the mode falls to `Unknown`, which reports side-effectful,
and therefore `truly_treeshakeable: false` by construction (the full-package comparison is gated off) and
confidence capped at Medium.

**The strip can fail.** A package whose `dist/` is itself a junction (Windows) or symlink (POSIX) onto a
directory outside the package resolves, after canonicalization, to an entry that is not under its own root.

**MEASURED:** such a package declaring `["dist/index.js"]` gets `side_effects: true`,
`truly_treeshakeable: false`, Medium, while Rolldown dropped the entry's gated effect (45 B minified). The
badge contradicts the build its own number came out of.

**Why it is accepted:** the size is right. Rolldown resolves the link exactly as webpack does, so the bytes are
the bytes. Only the badge is wrong, and only for a layout (a package whose build output directory is a link out
of the package) that essentially nobody ships. A previous version of the code comment asserted this case could
not happen; that was never measured, and it is false.

**What would fix it:** carry the pre-canonical package-relative path alongside the entry, so the relative form
survives a link that the canonical form cannot express.

### G1: The negative-`error` Guard catches 14 of 18 spellings
**Status: Accepted** · The number is machine-pinned, not claimed

The Guard bans the `!result.error` usability check, the single root cause of the "transient becomes durable"
defect that recurred seven times (see [ADR-0006](adr/0006-the-result-model.md)).

It catches 14 of 18 planted spellings. The four misses are named in the test file with reasons (destructured
`const { error } = result`; a ternary; a bare `== null` expression; Rust `let Some(_) = ... else`). The count
is asserted, so a future change that silently weakens it fails the test.

**Static analysis is the second line here, not the first.** The real enforcement is that a degraded result has
no size to misuse: the size fields are `Option`, and the durability gate lives inside each store.

### G3: The workspace report reuses the "Combined Import Cost" header for a compressed and an uncompressed column
**Status: Accepted** · Each figure is correct; only the shared label spans two bases · Found in the 2026-07-16 module audit (E4)

The exported workspace report (`extension/src/ui/reportContent.ts`) prints three figures under the "Combined
Import Cost" label: the headline and the Duplicate Imports column render `combinedImportCostBrotliBytes`
(compressed, brotli), while the Shared Modules column renders `combinedImportCostBytes`
(`DuplicateModuleGroup.combinedImportCostBytes`, `ipc/protocol.ts:607-608`), which is rendered, uncompressed
bytes. So one header names a compressed figure in two tables and an uncompressed one in the third.

**What actually happens.** Every individual number is correct for the quantity it represents (the shared-module
figure genuinely is the uncompressed per-site upper bound), and that basis is disclosed verbatim by the note
printed directly above the table ("It is an upper bound, never a size. Rendered (uncompressed) bytes."). The
only harm is a reader who ignores the note and cross-reads the identically-headed columns between tables, a
label and presentation ambiguity, never a wrong number, wedge, or stale read.

**Why it is not fixed:** presentation-only (a pure HTML render), and the basis is disclosed inline.
**What would fix it:** give the Shared Modules column its own basis-explicit header (for example "Rendered
(uncompressed)") instead of reusing the compressed "Combined Import Cost" label.

### G4: Changing the compression format does not re-render the status-bar size until the next edit
**Status: Accepted** · Stale but correctly labelled, never a wrong number · Found in the 2026-07-16 module audit (E7)

Changing `importLens.compression` routes through the generic `affectsConfiguration("importLens")` arm to a
`uiOnly` refresh (`configChange.ts:24`, then `extension.ts:352`, then `configRefresh.ts:44-52`), which
reapplies decorations and insights but does NOT call `actions.schedule`, so `analyze` and `updateFileSize`
never re-run and the status bar keeps the figure from the prior analysis.

**What actually happens.** The frozen figure is shown with its own (old) compression label, for example
`IL: 1.2 kB brotli`, a correct brotli count correctly labelled brotli, just not yet recomputed in the
newly-selected format. It self-heals on the next edit or editor focus change (`onDidChangeActiveTextEditor`
then `schedule`). No wrong number, no wedge; the label discloses the basis.

**Why it is not fixed:** presentation-only, self-healing, basis disclosed (the R1/S1/G3 class).
**What would fix it:** have the `uiOnly` compression-change path recompute the visible size label for the new
format (it already holds every compression's bytes per import).

### K3: Disk-cache bookkeeping (summary byte total, shard id) is best-effort and can drift
**Status: Accepted** · Feeds eviction and observability only, never an import number · Found in the 2026-07-16 module audit (D5)

Two independent bookkeeping approximations, neither on the number-serving path:

- **Summary `total_bytes` drift.** `heal_summary_if_inconsistent` (`disk.rs:1048-1081`) rebuilds only when
  `cache_len != summary_count`; a `total_bytes` underflow silently floors at 0 (`write_summary` `.max(0)`,
  `disk.rs:1446`) with the count still correct, undetected until a full rescan. It feeds
  `ProjectCacheStatus.total_bytes` and the eviction budget: a low value under-evicts (disk overage), a high
  value over-evicts (extra rebuilds), both cost only rebuilds, never a wrong served number.
- **Shard-id collision.** `project_cache_shard_id` (`project.rs:982`) is 64-bit FNV-1a; two roots can map to
  one redb shard. Entries stay isolated (keyed by `package_root` and `entry_path`), so no cross-read of a wrong
  number; a read only crosses projects when both resolve the identical absolute entry (the same bytes, so the
  shared measurement is correct). Effect is limited to co-mingled cache-management display and a shared eviction
  budget; a 64-bit collision over a user's projects is negligible.

**Why it is accepted:** the cache is rebuildable and keyed by dependency fingerprints, so bookkeeping drift can
waste rebuilds or disk but can never surface a wrong import cost or lose a durable answer.

### I1: A rare wire-level failure degrades gracefully (connection teardown or dropped reply), never a wrong number
**Status: Accepted** · Found in the 2026-07-16 module audit (D6)

Two graceful-degradation paths in the daemon's connection loop, neither able to corrupt a number:

- **Oversized or malformed frame tears the connection.** A frame-decode `Err` (for example larger than
  `MAX_FRAME_BYTES` = 32 MiB) calls `close_connection` and returns (`server.rs:526-539`), unlike the
  payload-decode arm which `continue`s. But `close_connection` runs `wait_for_active_tasks` then
  `flush_cache()` unconditionally first (`server.rs:1285-1307`), so no measured result is lost and the
  extension respawns the daemon. A trusted client on the mirrored TS codec does not emit a 32 MiB frame.
- **A reply that fails to serialize is dropped.** `queue_outbound` logs and returns on a
  `rmp_serde::to_vec_named` `Err` (`server.rs:340-350`) with no retry; the client's `request_id` stays
  unanswered until its own timeout, showing Loading or timeout, never a wrong size. Dropping one frame (rather
  than tearing the connection) preserves the warm cache and every other in-flight request. `to_vec_named` on
  these plain `String`, `u64`, `Vec`, `Option` structs does not fail in practice.

**Why it is accepted:** both are last-resort paths for inputs a trusted client does not produce, and both fail
toward "no answer" (client retries, daemon respawns), never toward a fabricated or misrouted number. Handler
panics are already converted to routed protocol errors (`response_from_join`), so a panic does not wedge a
batch either.

### E2: Windows ARM64 (`win32-arm64`) is a declared target with no shipped binary or hash, so the daemon never starts
**Status: Accepted** · Fail-safe · Out of the current release scope (Windows x64) · Found in the 2026-07-16 module audit (E1 module)

`scripts/targets.mjs` and `platform.ts` resolve `win32-arm64`, but `knownHashes.generated.ts` ships no row for
it (5 keys, none `win32-arm64`) and no binary is built for it. On a Windows ARM64 host, `#verifyBinary` finds
no trusted hash, logs "No trusted hash", and `start()` sets the daemon `unavailable`, so the extension shows
nothing.

**Why it is not fixed:** the release is deliberately scoped to Windows x64 (AGENTS.md); refusing to launch an
unshipped or unverified binary is the correct fail-safe (it never produces a wrong number and cannot wedge).
**What would fix it:** add `win32-arm64` to the build and package matrix plus hash refresh when Windows ARM64
becomes a supported target. (Windows x64 also runs on ARM under emulation.)

---

# Priority 3: accepted by design

Deliberate consequences of the design. These are conservative by construction (they flag nothing rather than
invent a number) or bounded behaviours we chose. Revisit only if the blast radius changes.

## Path aliases

All four degrade to a **floor** (the file's total is flagged incomplete, is not cached, and `importlens check`
declines to judge it). A floor is conservative: it is never a wrong number. That is why none of them is fixed.

### A1: An alias declared only in a Vite, webpack, or Rollup config is not seen
**Status: Accepted** · The only one with real-world reach

We read `paths` from `tsconfig.json` and `jsconfig.json` (and their `references` and `extends`). An alias
configured only in a bundler config is invisible, so the file is a floor.

Narrow in practice: a TypeScript project must mirror aliases into tsconfig anyway or the editor breaks. A
JavaScript-only Vite project with no `jsconfig.json` is the real exposure.

**Repair for a user:** mirror the alias into `tsconfig` or `jsconfig` `paths`.

### A2: More than 24 reachable configs, the tail is not walked
**Status: Accepted** · `MAX_REACHABLE_ALIAS_CONFIGS = 24`

The `references` walk caps at 24 configs. Beyond that, an alias declared in the 25th is not seen, so a floor.
The nearest config is normally a package's own, so a huge solution-style root is rarely the one walked.

### A3: Cross-project alias contamination
**Status: Accepted** · A deliberate consequence of the design

We ask every reachable `paths` table, so an alias declared only in `tsconfig.node.json` will resolve for a
document governed by `tsconfig.app.json`.

This is the price of making the answer document-independent, which is what fixed the `.vue`, `.svelte`,
`.astro` breakage: asking "which project owns this document?" is exactly the question that kept producing
regressions. It errs toward "flag nothing" and cannot invent a number.

### A4: A tsconfig edited while the VS Code watcher is not running is not seen
**Status: Accepted**

Alias-table invalidation rides the extension's file watcher. A tsconfig changed outside a running VS Code
session is stale until the daemon restarts. `importlens check` is unaffected: the CLI spawns a fresh daemon per
run.

## Engine and concurrency

### C1: A package that reliably parks the bundler re-parks on every analysis
**Status: Accepted** · Bounded, and the alternative was worse

A build can park forever (Rolldown spawns its module tasks; the async runtime swallows their panics, so the
loader waits for a completion message that never arrives). `BUILD_TIMEOUT` (8s) stops it holding an engine
permit for good.

Its `timeout` result is, correctly, never cached (a transient failure must not become a durable answer), so a
package that reliably parks pays 8s again on each analysis. Two such packages can hold both engine permits
while the user types; other documents' imports wait, but no response is ever late, because imports stream.

A per-entry circuit breaker was tried and deleted: it durably condemned healthy packages that had merely been
slow once. Do not reintroduce it.

### C2: A cancelled build's module graph outlives its permit
**Status: Accepted**

On timeout the future is dropped and the permit released immediately, but Rolldown's already-spawned module
tasks keep running and hold the parsed graph. So peak RSS can briefly reach about 3 graphs rather than the 2
the permit count implies. Bounded (the tasks do complete) and it cannot wedge or corrupt.

### C3: `AnalyzeSpecifiers` still blocks on engine misses
**Status: Accepted** · Recorded as SRS FR-004b

The Compare-imports command and named-export candidates are one-shot commands with no `AnalysisStore` rows for
a streamed push to merge into, so streaming them would hand the UI an empty list with nowhere for late results
to land. They block, and with `EngineBudget` deleted they carry no total time bound.

A fabricated comparison would be worse than "comparison failed."

### C4: Cross-request response ordering is no longer guaranteed
**Status: Accepted** · A consequence of the multiplexing connection loop

Two pipelined requests may now be answered out of order. Nothing in the extension depends on it (every response
is routed by `request_id`), but it is a protocol-level behaviour change.

### C5: Shutdown can take up to `BUILD_TIMEOUT`
**Status: Accepted**

Shutdown joins in-flight handlers under a bounded deadline, then flushes the cache unconditionally. A build
already inside Rolldown cannot be cancelled, so a parked one can hold shutdown to its 8s limit. A task still
running at the deadline is abandoned and its result is not persisted, stated in the SRS rather than papered
over.

### C6: A nested `"type"` does not reach the pre-resolved entry (dual-package layouts)
**Status: Accepted** · One field, two lookups, no fix exists at the current upstream API

The plugin supplies the package-root `package.json` for the entry it pre-resolves
(`HookResolveIdOutput::package_json_path`). Rolldown then makes two different lookups against it, and the field
can only be right for one:

| lookup | manifest Rolldown wants | our supply |
| --- | --- | --- |
| `sideEffects` | the topmost manifest before the `node_modules` boundary (`find_package_json_for_a_package`), the package root | correct |
| `"type"` (module format) | the NEAREST manifest above the file (`esm_file_format`) | correct only when no manifest intervenes |

**What actually happens.** Take the standard dual-package layout: root `package.json` is
`{"main":"./esm/index.js"}` with no `"type"`, and a nested `esm/package.json` is `{"type":"module"}`, whose
entry statically imports a CJS dependency. The same package emits two different chunks depending on how it is
reached (measured in-repo, unminified chunk):

```js
// reached TRANSITIVELY (Rolldown resolves the file, finds esm/package.json): 1333 B
var import_dep = /* @__PURE__ */ __toESM(require_dep(), 1);

// reached as the PRE-RESOLVED ENTRY, the production shape: 1330 B
var import_dep = /* @__PURE__ */ __toESM(require_dep());
```

The `isNodeMode` flag is what makes the namespace's `default` the whole `module.exports` object, which is what
Node does for an ES module importing CommonJS. Without it the entry is finalized as a CommonJS importer: a
different `default` binding and a different measured size.

**It is not a regression.** With no manifest supplied at all (the pre-`f2bdc17` behaviour) this layout emits
the identical 1330 B chunk: the entry's format was `Unknown` then and is decided from a `"type"`-less root
manifest now. Supplying the root manifest closed the `sideEffects` half of the hole and left this half exactly
where it was.

**Why it is not fixed.** Swapping in the nearest manifest would break the `sideEffects` half, the half that
stops a `"sideEffects": false` package's entry keeping statements Rollup and webpack drop, which is a strictly
larger error on a far more common layout. There is no third option through this API.

**What would fix it:** an upstream Rolldown resolve-hook field that accepts the nearest manifest separately from
the package-root one; or resolving the entry through Rolldown instead of pre-resolving it, which FR-017 and
section 6.1 forbid (the engine must never re-resolve the bare specifier). Recorded in SRS section 10.7.

### D5: `importlens check` exit 3 will become common
**Status: Watch**

Any changed file with an unmeasurable import now exits 3, "could not measure", rather than silently passing.
That is deliberate: a gate that cannot measure must never report success, and a silent pass merges the
regression. But it is a real workflow cost, and if it proves noisy the answer is to make fewer imports
unmeasurable, not to make the gate lie.

---

# Performance backlog

From the release review's improvement list. All real; none blocking. Each is a known cost, not a defect.

| # | Item |
| --- | --- |
| P1 | **Prewarm priority inversion.** A user typing an import can queue behind two in-progress prewarm builds. Reserve an interactive permit. |
| P2 | **Answer `CacheProbe::Unresolved` in the classify pass.** Types-only, node-builtin and unresolvable imports construct no bundler, yet route through the engine drain. |
| P3 | **Drop the per-module source clone.** The graph's source is copied once per build for nothing. Hash first, then move the buffer. |
| P4 | **Avoid copying the linked chunk.** A multi-megabyte `clone()` purely to move it into the artifact. |
| P5 | **LRU the dependency-path index.** Capped at 32 entries with an arbitrary eviction victim; a monorepo thrashes it and first-party freshness degrades nondeterministically. |
| P6 | **`drain_ordered` uses 2 workers where `drain_classified` uses 4.** package.json analysis and both prefetch drains idle a permit with work queued. |
| P7 | **Rebuild fixed option data once, not per build.** About 180 `String`s allocated per build; `LazyLock` candidates. |
| P8 | **The miss drain spawns fresh OS threads per call.** A single cache miss spawns a thread to do work the caller could do inline; a 500-file report can perform hundreds of thread creations. |
| P9 | **The completion path re-verifies a whole package graph on every popup.** Re-reads and re-hashes every non-`node_modules` file per keystroke inside an import's braces. |
| P10 | **`ENGINE_PERMITS` is 2, tried at 4 (Task 13), measured, reverted.** Not deferred; see the outcome below. |

**P10 outcome (Task 13, measured 2026-07-15, reverted).** Raising `engine_permits()` to
`available_parallelism().clamp(2, 4)` was implemented and measured against the section 10.6 gate on an 8+-core
Windows machine (release, single runs):

| | permits=2 | permits=4 |
| --- | --- | --- |
| 20-import wall | 943 ms | 883 ms (-6%) |
| 20-import peak RSS | 82 MB | 137 MB (+67%) |
| cold p95 | 105 ms | 107 ms |

Both pass the 400 MB and 500 ms gate. But the wall-time gain is within single-run perf noise, while the RSS
rise is structural (more permits means more concurrent resident graphs). The bottleneck at this core count is
core saturation, not permits: each build already runs an 8-wide Rolldown runtime, so two concurrent builds
oversubscribe the cores and a third or fourth adds resident memory without throughput. Reverted: a real memory
cost for a noise-level speed gain, in the C7 concurrency area, does not earn its place. The win would
materialise only where the runtime width does not already saturate cores (much higher core counts, or lighter
builds); revisit only if the runtime-width versus permit split is reworked. The change was clean (const to
`LazyLock` semaphore plus `miss_drain_workers()`, all sites converted, tests green): the code is not the
problem, the tuning simply did not pay off here.

---

# Resolved and historical

Kept for the record.

### B2: The Import Cost ignored shipped non-JS asset bytes (CSS, wasm, fonts)
**Status: Resolved (2026-07-17)** · Fixed by `fix(analysis): count a package's shipped CSS, wasm, and font bytes` · Design: [asset-counting-design.md](asset-counting-design.md) · Plan: [asset-counting-plan.md](asset-counting-plan.md)

A package's real cost is not only its JavaScript. The engine measured the JS chunk and recorded a reachable
stylesheet's raw bytes as an `uncounted_asset`, disclosed beside the result but never folded into the Import
Cost and never processed as it would actually ship, so every CSS-shipping package undercounted.

The load boundary now classifies each non-JavaScript module as a stylesheet, wasm, font, or passthrough and
stubs the first three, so the JavaScript chunk still measures exactly. Every reachable stylesheet becomes one
artifact via Lightning CSS, which resolves the `@import` tree and minifies it, mirroring how CSS ships and how
the esbuild oracle emits a single sibling stylesheet; wasm and fonts are counted raw. Each artifact is
compressed on its own and summed ([ADR-0005](adr/0005-a-runtime-is-an-artifact-boundary.md)), in both the
single-import and the per-runtime File Cost paths. The result carries a per-kind `asset_breakdown` so the number
is legible, and a stylesheet that processes cleanly no longer costs the package its confidence. Any processing
failure falls back to the old raw-byte disclosure, so the result is a strict improvement or a tie, never a
regression ([ADR-0006](adr/0006-the-result-model.md)). `ANALYZER_REVISION` moved to `rolldown-1.1.x+5`.

Verified against the esbuild oracle on `@uiw/react-md-editor`, the only real package whose published ESM entry
imports CSS: the minified totals agree within 1%, so both sides fold in the same stylesheet exactly once. The
residual limits it left behind are D7, D8, and D9 below.

### B1: An all-inline-`type` named import was measured as the whole package
**Status: Resolved (2026-07-16)** · Fixed by `fix(daemon): stop sizing an all-inline-type import as the whole package`

`import { type Config } from "tailwindcss";`, a braced import whose every specifier carried the inline `type`
modifier, is erased by TypeScript to zero runtime cost, but was reported as an `import * as ...` namespace of
the whole package and summed into the file's Combined Import Cost. oxc marks the specifier entry `is_type` while
leaving the module request `is_type = false`, so the static-import loop dropped the entry and the
`requested_modules` fallback resurrected the statement as a namespace. The loop now registers such a statement
in `elided_statements` so the fallback cannot resurrect it; a regression test pins an all-inline-type import to
zero detected runtime imports. `ANALYZER_REVISION` moved to `rolldown-1.1.x+4`.

### B3: Native-binary-backed packages were mismeasured instead of labelled
**Status: Resolved (2026-07-16)** · Fixed by `fix(analysis): label native-binary packages rather than mismeasure them`

A package that ships a platform-specific native binary as `optionalDependencies` (Biome, the TypeScript 7
native rewrite, esbuild) either showed a bare "unavailable" (no importable JS entry) or a confident,
misleadingly tiny size for a JS shim (TypeScript 7's 113 byte version stub, measured at roughly 867 bytes). The
daemon now detects the platform-suffixed `optionalDependencies` convention at the resolver boundary and labels
rather than counts: a package with no importable JS entry is a measured zero with a "native binary only" badge;
one whose entry resolves keeps its measured size with a "native binary" flag beside it. Requiring the manifest
to declare no entry field keeps a broken install honestly "unavailable". `ANALYZER_REVISION` moved to
`rolldown-1.1.x+4`.

### K1: The `sideEffects` badge fix is invisible on a warm cache until `ANALYZER_REVISION` moves
**Status: Resolved (2026-07-15)** · Task 14 bumped `ANALYZER_REVISION` to `rolldown-1.1.x+3`

**What actually happened.** A user who analysed `react-loading-skeleton`, or any package declaring a
`sideEffects` glob or `[]`, on a pre-fix daemon held a persisted entry that said `side_effects: true`,
`truly_treeshakeable: false`, Medium. That is the exact wrong badge `8f607a0` and this commit exist to abolish,
and they kept being served it.

Nothing re-examines it. `ImportResult::is_durable()` is an insert-time gate: it decides what may enter a store,
and a stored entry is never re-validated on read. The only thing that rejects an entry computed by older code
is its `CacheIdentity.analyzer_version` (`ANALYZER_VERSION` = crate version plus `ANALYZER_REVISION`,
`cache/key.rs`), which both stores check on read and which `purge_orphan_entries` sweeps on. The analyzer
changed; the identity did not.

**Why it was bumped once, not per fix.** `ANALYZER_REVISION` is bumped once for the whole bundler-redesign
batch, by Task 14: bumping it per fix would throw every user's cache away several times over the branch, and
the disk schema stays at 8 meanwhile.

**Resolution.** Task 14 bumped `ANALYZER_REVISION` from `rolldown2` to `rolldown-1.1.x+3` (`cache/key.rs`),
which lands with every measurement-affecting change on this branch. Every entry computed by the pre-fix daemon
now fails the `analyzer_version` check on read and is re-measured, so the corrected badge reaches existing
installs, not only brand-new caches. This was the hard dependency the whole batch of badge and size fixes rode
on; it is discharged. (The B1 and B3 fixes bumped it again, to `rolldown-1.1.x+4`; B2 will bump it once more when it lands in its own pull request.)

### U1: `a6cae06` did not get an adversarial review
**Status: Resolved (2026-07-16)** · Covered by the module audit's D2 review

The commit that hoists the alias resolver construction out of the per-specifier loop (a 7x interactive-path
regression fix: 27.96 ms to 7.27 ms at 20 aliased imports) was dispatched without the independent verify pass
every other commit on this branch received.

It self-proved the property that matters (the sticky-floor test still goes red if the resolvers are
re-memoised) and the full suite passes. It was recorded here because every other round on this branch had a
Critical found by independent verification.

**Resolution.** The 2026-07-16 whole-codebase module audit reviewed `resolver.rs` in full under the D2 module
(finder plus a fresh adversarial verifier), which covers the a6cae06 change. No wrong-number or wedge defect
was found in the resolver; the two D2 findings (R1, R2) are a badge-only issue and a non-reproducible ordering
inconsistency, both recorded above.
