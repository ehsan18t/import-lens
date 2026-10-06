# Known Issues

The full tracker of known issues on this project: release blockers that must be fixed before shipping,
work deferred for later, and behaviours we have accepted and are content to leave. Everything found and
not yet resolved is recorded here so nothing is lost. Entries are ordered by priority, highest first.

## How to use this file

Record an issue here when you decide how to treat it, not merely when you find it. A **Blocker** must be
fixed before release. A **Deferred** item is worth doing and should become a task. An **Accepted** item we
are content to leave. Every entry states what actually happens and why it is treated the way it is. An entry
with no failure scenario is a rumour, and a rumour in a tracker is worse than nothing.

**Delete an entry when it is fixed.** This file answers one question — what is wrong with the product right
now, and what did we decide about it — and every resolved entry left in place makes that question harder to
answer. Fixing something and writing a paragraph about the fix here grows the file forever and buries the
things that are still true. When an issue is resolved, delete its entry: the behaviour belongs in the SRS,
the reasoning in the commit, and the guarantee in the test. Do not cite a tracker ID from code or other
documents; state the behaviour instead, so nothing points at an entry that has been deleted.

A decision **not** to fix is not a resolution. An Accepted or Deferred entry stays, in full, because it is
still true of the product — and so does a documented decline, which exists to stop the same dangerous change
being attempted twice.

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

---

# Priority 0: release blockers

Nothing is open.

---

# Priority 1: deferred, worth doing

Real work, queued rather than abandoned. None of these is a wrong number or a wedge today, so none blocks the
release, but each is worth turning into a task.

### D6: "Unavailable" is one label for several causes, and one unbundleable leaf discards the whole package
**Status: Deferred — RESOLVER scope** · The most visible gap in real projects · Fix universally, never per-package

**What the user sees.** In a real `package.json`, some dependencies render **unavailable**. The bigger the
project, the more of them, which reads as "the build was too big." It is not a size problem.

**What still lands here.** Two classes remain:

- **No importable entry.** A package declaring no `main`/`module`/`exports`/`browser` at all — confirmed on
  `@next/font`, whose real code is subpath-only (`./google`, `./local`) — is **Unmeasured**, and correctly so:
  importing that specifier does not cost nothing, it does not resolve at all, and a zero would be a fabricated
  number. The message now names the reason and lists the importable subpaths. What is open is the **stage**: it
  is still the generic `entry_resolution`, so the badge cannot distinguish this from a broken install.
- **An unfollowable dynamic `require`.** A leaf a bundler cannot statically resolve still fails the whole
  package build, so a 2 MB graph with one such edge reports nothing rather than "at least 2 MB, excluding it."

**The universal fix (never per-package).** At the engine/resolver boundary, treat every unbundleable leaf as an
import boundary rather than a hard failure: measure the graph that did bundle as a **floor**, and disclose the
uncounted leaf exactly as non-JS asset bytes are disclosed today. Same shape as D2 — a floor beats a blank —
but triggered by a build error rather than a graph-limit breach. **This shape now has a working instance to
copy:** an unmatched import binding *between two dependencies* is stubbed, measured as a floor, and disclosed
under `missing_export` (SRS failure-stage table), which is what makes a package like `tsdown` measurable at all.
The remaining classes above want the same treatment, and the constraint that instance had to respect is the one
they will meet too — the leniency must never extend to the thing the **user requested**, or a typo comes back
as a confident number. Pair it with a labelled reason in the UI
("no importable entry", "unresolved: X") so the badge names the truth instead of a blanket "unavailable". Two
probes already answer with a labelled Measured zero (`types_only`, `native_binary_only`); note
`native_binary_only` ships with no SRS requirement behind it, so if a dedicated stage is added here, charter
both at the same time. Do this at the boundary so it covers every package by construction; do NOT special-case
any named dependency.

**Why it is not a blocker.** "Unavailable" is honest: no wrong number is shown, and nothing can wedge. This is
a coverage and UX upgrade, not a correctness fix. The suspicion that a pure-JS package "should measure and does
not" was flushed out — every instance traced to a genuine unresolvable or unbundleable leaf, not a measurement
bug.

**Prerequisite: get the distribution before designing the fix.** A diagnostic that runs the daemon over a real
project's whole dependency set and buckets each "unavailable" by its actual stage (no-entry, dynamic-require,
graph-limit, timeout, parse) sizes the fix and surfaces any genuine bug. Build that first.

### D7: A stylesheet its own package declares droppable is counted anyway
**Status: Accepted** · A wrong number on a package shape measured to be absent from the real ecosystem · Found by the asset-counting adversarial review

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
retention would zero out the CSS for `@uiw/react-md-editor` and undo asset counting entirely. The honest fix asks the DECLARATION rather than the build, and that is exactly what this product forbids.
FR-021 (Critical) and the engine boundary contract both state that the daemon's own reading of `sideEffects` is
reporting metadata that "decides a badge, never a byte", and both name rolldown as the only authority on
retention. Dropping an asset on our reading makes it decide bytes. Closing D7 therefore requires amending a
Critical requirement, not writing code, and doing that quietly would be the narrow-the-spec-to-fit-the-code
failure this repository has been bitten by before.

Re-verified 2026-07-18, and the other two blockers are structural rather than incremental. Rolldown's
`HookLoadArgs` carries `id`, `module_idx` and `asserted_module_type` and no importer at all; `resolve_id` has
the importer and discards it on the success path, so the daemon keeps no asset-to-package mapping to build on
(the importer is read only in `plugin.rs`'s resolve hook and is never retained). Rolldown's own `ModuleInfo`
has an `importers` field, but it is empty when `module_parsed` fires, and whether it is filled after linking
is unverified. And the `sideEffects` patterns are collapsed to a bool deliberately: `resolver.rs` says that
collapsing them means nothing downstream can read them a second way, which is precisely the second reading D7
would need.

Accepted rather than Deferred, because Deferred says "worth doing, not now" and this is not queued work: it is
a measured non-shape in the ecosystem whose fix conflicts with a Critical requirement. Revisit only if the
ecosystem survey changes.

### D8: One stylesheet Lightning CSS cannot parse falls back alone, but a cyclic one undercounts
**Status: Accepted** · Never below raw-byte disclosure · Found by the asset-counting adversarial review

Lightning CSS parses plain CSS. A published package that imports a preprocessor source (`.scss`, `.less`) or a
stylesheet with a bare `@import "pkg/base.css"` cannot be bundled, so that sheet falls back to raw-byte
disclosure. That is the ADR-0006 fallback working: it lands exactly on raw-byte disclosure, never below it.
A failed set retries per sheet, so only the offender falls back and the rest stay counted. In that degraded
mode two sheets sharing an `@import` are no longer deduped against each other, which over-counts the shared
part, a smaller and rarer error than dropping them all.

A stylesheet caught in an `@import` cycle keeps its `@import`ed rules but loses its own, which undercounts that
one sheet. Cycles are silent in browsers and in every real bundler, so a package can ship one unknowingly. It no
longer threatens the daemon (that wedge is fixed and pinned by a regression test); it is now only an accuracy
edge on broken input, and still strictly better than not counting CSS at all, when the package contributed zero CSS either
way.

Do not resolve a bare `@import` with the JavaScript resolver: that profile has no `style` main field, no
`style` condition and no `.css` extension, so it would answer `pkg/base` with `pkg/base.js` and measure the
wrong file. Doing it properly needs a purpose-built CSS resolver profile.

### D9: A stylesheet's own `@import` tree is bounded at 256 files
**Status: Accepted** · A bound where there was none · Found by the asset-counting adversarial review

A stylesheet's `@import` children are never graph modules, so none of the engine's limits ever applied to them.
Lightning CSS recurses per `@import`, and a deep enough chain overflows the stack, which is NOT catchable: the
process dies rather than the import failing. One attempt is therefore bounded to 256 files and 8 MB, inside the
AC-03 build-wide 512-read/16 MiB ledger. A production union that consumes the per-attempt limit can exhaust that
shared ledger during retry, and a ledger breach ends as the disclosed floor: the JavaScript stands and every
collected asset is disclosed at its raw size under `uncounted_assets`, none counted. Tests that want to observe
the per-sheet degradation itself lift the ledger with `AssetBudgetLimits::unbounded_css_work`.

The file count doubles as the depth bound, because a chain of N files costs N reads. A separate depth bound is
possible (the provider's `resolve` sees every parent-to-child edge), but it buys nothing real: no real
stylesheet tree is broad enough to hit 256 files, and the build-wide ledger would cap it anyway. 256 stops the walk roughly three times short of where a release build's
stack gives out, and is far more than any real stylesheet's tree. It cannot simply be raised on the grounds that
a flat set of many sheets carries no stack risk: the bound cannot tell breadth from depth, and giving the walk
its own larger stack does not help either, because Lightning CSS drives the `@import` graph on `rayon` workers
whose stacks it does not own. Early structural union failures can still degrade into the per-sheet path, which
is disclosed as `imprecise_assets` and drops the result off High confidence.

That degraded number reads HIGH for two reasons, and the smaller one is the obvious one. Sheets sharing an
`@import` inline it once each. But each sheet is also compressed on its own, so no sheet's compressor can use
what the others contain, and that term dominates: 300 tiny stylesheets sharing no `@import` at all, which is the
shape that actually breaches a 256 file bound, sum to roughly 40x the union's gzip and 57x its brotli, because
every stream restarts its window and pays its own header. Real stylesheets are larger and fewer, so the real
factor is far smaller, but the direction is the same and it is not a small correction. This is why the
disclosure fires on the union having failed rather than on the sheets provably sharing bytes: disjoint sheets
are the worst case here, not the safe one.

The upper bound remains deterministic, cacheable, and useful to show, but it is no longer treated as exact by
any budget surface. Editor diagnostics, the workspace report, and `importlens check` share a coordinated
non-budgetable-stage list; `imprecise_assets` produces no pass or failure, and CI exits with its distinct
"could not evaluate" result instead of reporting a false regression.

The byte half of the budget is reserved from metadata before each read and reconciled with the exact bytes
afterward, so it bounds a tree's total. A single `@import` child is also held to the 20 MiB per-file limit on
module source (`MAX_MODULE_SOURCE_BYTES`) through the asset read ledger, though the 8 MB per-attempt bound and
the 16 MiB build-wide CSS work limit are both tighter, so one of those refuses an oversized child first. A tree
that breaches the budget is refused rather than mismeasured.

### D13: An image referenced from counted CSS is disclosed, not counted
**Status: Accepted scope** · Decided 2026-07-18 while fixing the silent-drop defect

A stylesheet's `url()` graph can reference kinds outside the processed taxonomy — images, SVG. Those bytes
ship, so they are disclosed at their real size under `uncounted_assets`, which makes the result a floor and
holds it at Medium confidence (FR-018b).

They are **not counted**, and the distinction is deliberate rather than technical. An image needs no processor
— its shipped size is its raw bytes compressed, exactly like a font — so counting it would be easy. What stops
it is that counting changes what the number *means* for a whole category of packages, and the esbuild oracle
and every accuracy baseline would have to be re-measured to confirm the two sides still agree on what a build
emits for an image reference. That is a measurement task, not a code change, and it is not this fix.

The cost of the current choice is real and should not be hidden: a UI kit shipping sprites reads Medium with a
floor rather than High with a total. That is the honest reading of what we know.

### D14: Runtime-fetched CSS resources are disclosed but never counted
**Status: Accepted scope** · Decided 2026-07-18

A CDN `@import` or a remote `url()` is disclosed on the `external` stage and excluded from the number. This
follows ADR-0004: the tool measures what an import *ships*, and a resource the browser fetches from another
origin is not shipped by this package. The measured size is therefore exact and keeps its budget verdict; only
confidence drops.

We do not fetch the resource to size it. Doing so would make a measurement depend on the network, make it
non-deterministic and non-cacheable, and let a package's reported cost change without any byte on disk
changing — all of which ADR-0006 exists to prevent.

### D9 follow-up: unioning the surviving stylesheets is NOT a safe improvement
**Status: Investigated and declined 2026-07-18** · Evidence below, revisit only after the prerequisite

The obvious improvement to the per-sheet fallback — re-union the sheets that parsed, so only the
offender is measured alone — was investigated and rejected on evidence.

`charge_css_work` is monotonic with no per-path dedupe, and union-plus-retry already spends roughly
2x the set's reads against a build-wide 512-read / 16 MiB ledger. A third pass makes it ~3x, and
breaching that ledger is **terminal for the asset stage**: every stylesheet that would have counted is
disclosed at its raw size instead, so the whole asset contribution drops to a floor.

Worse, the common reason the union fails IS the set breaching a budget together — that is exactly the
shape the regression test at `assets.rs` pins (two sheets, each inside the 256-file per-attempt bound,
breaching it together). In that case the "survivors" are all the sheets, so re-unioning them simply
breaches again, having spent a third of the ledger to learn nothing. And
`may_retry_stylesheets_separately` is a single negative test on the COMPRESSION stage, so there is no
signal distinguishing "one unparseable sheet" from "the set was too large".

The trade would therefore be a **disclosed** over-count (already non-budgetable, already labelled as
reading high) for a possible breach that leaves no stylesheet counted at all. The prerequisite is a typed
distinction between a per-sheet parse failure and a set-level budget breach; until that exists, this
is not worth attempting.

### D7 follow-up: per-asset `sideEffects` attribution is further away than recorded
**Status: Still deferred, with a corrected blocker** · Re-examined 2026-07-18

D7's recorded blocker is "needs per-asset package attribution first". That was read as meaning the
attribution was the only missing piece. Re-examination found three separate blockers:

- Rolldown's `load` hook has **no importer parameter**. `args.id` is the asset's own id; the importer
  exists only in `resolve_id`, which discards it. Nothing in the daemon maps an asset path back to the
  module that imported it (Rolldown's `ModuleInfo::importers` is empty at `module_parsed`).
- `sideEffects` patterns are **collapsed to a bool at parse time** (`SideEffectsMode::Array { entry_matches }`).
  The patterns are deliberately not retained, so the value cannot be re-asked about a different path.
- The engine boundary contract states the daemon's own reading of `sideEffects` is "reporting
  metadata — it decides a badge, never a byte". Dropping an asset on that reading makes it decide
  bytes, which is the thing the contract exists to prevent.

### D18: A CSS `url()` may resolve outside the package root
**Status: Accepted** · Decided 2026-07-18

A relative `url("../../../fonts/x.woff2")` in a stylesheet is resolved and canonicalized with no check
that the result stays inside the package that declared it, so a reference can escape into a sibling
package or above `node_modules` entirely.

Not fixed, and deliberately: containment is not an invariant this tool holds anywhere else. The graph
already loads and measures whatever JavaScript an entry imports, from wherever it resolves, and
`node_modules` is trusted build input by construction. Adding a boundary here would enforce it in one
narrow place while every other read ignores it, which buys no safety property.

It would also cost accuracy. A monorepo package legitimately referencing a shared font through `../`
is a real shape, and a containment check would stop counting bytes that genuinely ship — turning a
correct number into a floor to prevent something that is not a defect.

### D19: The per-sheet retry is bounded, not free
**Status: Accepted bound** · Measured 2026-07-18

A stylesheet's `url()` dependencies are stat'd one at a time, and the loop checks the deadline before
each one, so the work is bounded by the same eight-second budget as everything else in the stage. The
stats are not charged to the byte ledger because a stat moves no bytes.

Recorded rather than fixed because the bound already exists and adding a second accounting mechanism
for zero-byte operations would be more machinery than the risk justifies.

### D28: A counted CSS resource is canonicalized more than once per build
**Status: Accepted** · Measured 2026-07-19, narrowed 2026-10-06

`collect_referenced_assets` examines each file a stylesheet's `url()` references name once per collection, so an icon-font sheet naming three files from fifty rules pays three canonicalize-and-stat pairs, not fifty. What remains: a first-time counted font or wasm file is canonicalized twice more by the ledger's snapshot (`snapshot_if_present`, then `snapshot`), and each per-sheet retry collects its own sheet's references again. On Windows a canonicalize is a file-handle open, about 0.12 ms per reference as measured when every repeat paid it.

Accepted: it moves no number and can wedge nothing, and a memo on the snapshot path sits directly on the read that freshness is derived from, which is worth more care than a fraction of a millisecond buys. Revisit if the asset stage shows up in a p95 regression.

### D24: A loader suffix on a non-JavaScript, non-asset module still fails the build
**Status: Accepted** · Blocker identified 2026-07-19

The load hook strips a loader suffix from any module id and falls back to the literal path when the
stripped one is not on disk, so `./font.woff2?url` (asset, stubbed) and `./util.js?v=1` (JavaScript,
parsed) both measure. `./payload.json?raw` does not: the file is found, but **Rolldown infers module
type from the id's extension**, and `payload.json?raw` is not `.json`, so JSON text reaches the
JavaScript parser and the build fails with `PARSE_ERROR` instead of the earlier filesystem error.
Same outcome for the package, different stage.

Not fixed because both routes cost more than the shape is worth. Overriding the module type needs an
extension-to-`ModuleType` table, which duplicates inference Rolldown already owns — the second
mechanism this codebase's rules exist to prevent. Stripping the suffix in `resolve_id` instead would
let Rolldown's own inference see a clean id, which is the right shape, but that arm currently fires
only for asset specifiers and widening it changes resolution semantics for every module in the graph.

Revisit if a real package ships this; `?raw` and `?url` are app-author vocabulary, and a sweep of two
project trees found no resolvable instance inside `node_modules`.


### D2: An honest lower bound on a failed build
**Status: Deferred** · The intended successor to ADR-0003

Today an unbuildable import reports no size. A graph-limit breach means much of the graph was loaded before we
stopped, so a real floor exists: "at least 4 MB; graph limit exceeded" is strictly better than a blank. The
engine currently discards the partial graph on failure, so this needs plumbing through the engine boundary.

### D31: A shard directory is removed without checking whether another process holds it
**Status: Deferred** · Data-loss class on unix; benign on Windows, which is the supported platform

`remove_shard_by_id` (`project.rs`) calls `fs::remove_dir_all(&cache_path)` unconditionally, reached from
the Manage Cache commands and from the automatic orphan sweep. There is no check
for another process holding the shard's `.redb`.

On **Windows** `redb` opens through `std::fs::File`, which does not request `FILE_SHARE_DELETE`, so the
delete fails with a sharing violation and is reported as `removed: false`: annoying, not destructive.
On **unix** the unlink succeeds while the holder keeps writing to an unlinked inode, and that data
vanishes on close. A related ordering hazard exists on both: `remove_dir_all` deletes in readdir order,
so a failed `.redb` delete can still leave the JSON sidecar gone, producing a metadata-less shard
directory that is invisible to every listing yet still counted by the maintenance gate.

**Why it is not fixed:** unreachable in practice on the supported platform, and reaching it at all
requires two windows on one workspace (both resolve the same `storageUri`, so they share a cache base). Both the unix unlink and the sidecar ordering become
routine under a shared cache base, so the fix is sequenced with that work rather than ahead of it.

### D4: A file with one unmeasurable import can never cache its total
**Status: Deferred** · A performance cost of an invariant we want

An aggregate missing a contributor's bytes is a **floor**, and a floor is never cached. So a file containing
one permanently-broken import, one deterministically unprocessable supported asset, or one import measured as
a floor (an unresolvable specifier kept as a boundary, a stubbed binding) re-runs its combined build and asset
tail on every size request, and `importlens check` declines to judge it (exit 3, D5). The per-import deterministic outcome is still cached; the file
aggregate cannot be, because it is not a complete File Cost.

A memo of the deterministic build failure was implemented and reverted (2026-10-06): it is not safe with the
inputs a failed build records. A relative JavaScript module that is missing when the build runs fails it at
`resolve`, a durable stage, but the plugin records an absent-file fingerprint only for asset candidates, so the
memo never expires when the module appears; a module deleted and recreated mid-build (`rimraf dist && tsc`, a
checkout) is memoized the same way; and a workspace tsconfig error is durable `link` while a tsconfig edit
bumps no generation. Each served a stale floor indefinitely where the uncached path recovers on the next
request. **Prerequisite for a retry:** absent-file fingerprints for every module the build failed to read or
resolve, and a generation bump on tsconfig edits.

### D3: Marginal cost, a project-level bundle model
**Status: Deferred** · A different product, decided on its own merits

"Adding `zod` here costs nothing, it's already in your bundle." Import Lens measures **imports, not bundles**
([ADR-0004](adr/0004-import-lens-measures-imports-not-bundles.md)) and has no model of what is already in the
bundle. Answering this means building that union model. It is the highest-value idea absent from the design,
and it must be a deliberate decision, not smuggled in as a bug fix.

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

### G1: The negative-`error` Guard catches 18 of 24 spellings
**Status: Accepted** · The number is machine-pinned, not claimed

The Guard bans the `!result.error` usability check, the single root cause of the "transient becomes durable"
defect that recurred seven times (see [ADR-0006](adr/0006-the-result-model.md)).

It catches 18 of 24 planted spellings (`STATED_COVERAGE` in `scripts/test/result-model-guards.test.mjs`). The
misses are named in the test file with reasons. The count is asserted, so a future change that silently weakens
it fails the test.

**Static analysis is the second line here, not the first.** The real enforcement is that a degraded result has
no size to misuse: the size fields are `Option`, and the durability gate lives inside each store.

### K3: Disk-cache budget is enforced on logical bytes, and shard ids can collide
**Status: Accepted** · Feeds eviction and observability only, never an import number · Found in the 2026-07-16 module audit (D5)

Two independent bookkeeping approximations, neither on the number-serving path:

- **Budget enforced on logical bytes, gated on physical ones.** `run_maintenance` skips its pass while the summed `.redb` file sizes are within `cacheMaxSizeMB`, but `BudgetCoordinator` evicts only until the summed value bytes reach the low-water mark. File size also carries keys (stored twice, with the recency index), B-tree pages and free pages, so the cache can sit with values under budget and files over it. The pass then compacts at a zero fragmentation threshold and, if the files are still over budget, logs a warning naming both figures; it does not evict further. Maintenance runs once per connection (60 s after the Hello), so this costs one redundant pass per connection, not a loop. Manage Cache shows both `total_size_bytes` (physical) and `total_bytes` (logical). The SRS settings table calls `cacheMaxSizeMB` a disk-byte budget, which the files can exceed by that overhead.
- **Shard-id collision.** `project_cache_shard_id` (`project.rs`) is 64-bit FNV-1a; two roots can map to one redb shard. Entries stay isolated (keyed by `package_root` and `entry_path`), so no cross-read of a wrong number; a read only crosses projects when both resolve the identical absolute entry (the same bytes, so the shared measurement is correct). Effect is limited to co-mingled cache-management display and a shared eviction budget. Widening the id would orphan every existing shard for a negligible risk.

**Why it is accepted:** the cache is rebuildable and keyed by dependency fingerprints, so bookkeeping drift can waste rebuilds or disk but can never surface a wrong import cost or lose a durable answer. A summary byte total driven negative is drift and is rebuilt from a scan in the same transaction.

### I1: A rare wire-level failure degrades gracefully (connection teardown or dropped reply), never a wrong number
**Status: Accepted** · Found in the 2026-07-16 module audit (D6)

Two graceful-degradation paths in the daemon's connection loop, neither able to corrupt a number:

- **Oversized or malformed frame tears the connection.** A frame-decode `Err` (for example larger than
  `MAX_FRAME_BYTES` = 32 MiB) calls `close_connection` and returns, unlike the payload-decode arm which
  `continue`s. `close_connection` cancels all cancellable work, waits for the invalidation to settle, aborts
  maintenance, joins in-flight tasks for up to `TASK_JOIN_TIMEOUT` (2 s), then calls `flush_cache()`
  unconditionally. Every result already measured is persisted; a build still running past the 2 s join is
  abandoned unmeasured, and the extension respawns the daemon. A trusted client on the mirrored TS codec does not
  emit a 32 MiB frame.
- **A reply that fails to serialize is dropped.** `queue_outbound` logs and returns on a
  `rmp_serde::to_vec_named` `Err` (through `codec.rs`'s `payload_bytes`) with no retry; the client's
  `request_id` stays unanswered until its own timeout, showing Loading or timeout, never a wrong size.
  Dropping one frame (rather than tearing the connection) preserves the warm cache and every other in-flight
  request. `to_vec_named` on these plain `String`, `u64`, `Vec`, `Option` structs does not fail in practice.

**Why it is accepted:** both are last-resort paths for inputs a trusted client does not produce, and both fail
toward "no answer" (client retries, daemon respawns), never toward a fabricated or misrouted number. Handler
panics are converted to routed protocol errors (`response_from_join`), so a panic does not wedge a request
either.

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

A1, A2 and A4 degrade to a **floor** (the file's total is flagged incomplete, is not cached, and `importlens
check` declines to judge it); A3 errs the other way and flags nothing. Neither direction invents a number. That
is why none of them is fixed.

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

### A4: A file total can keep an old alias classification for up to 30 seconds
**Status: Accepted**

The alias tables and their `references` graph are re-read on every request, so a `tsconfig.json` or `jsconfig.json` edit is seen on the next analysis whether or not the VS Code watcher reported it. The L1 file-total cache keys on the import classification (`path_alias` versus `not_installed`) and lives for 30 seconds, so an edit the watcher did not report (a referenced config outside the workspace folder) can leave a file total classified the old way until that entry expires. `importlens check` is unaffected: the CLI spawns a fresh daemon per run.

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
larger error on a far more common layout. `HookResolveIdOutput` also has a `side_effects` field, so the nearest
manifest could be supplied with `sideEffects` decided by the plugin, but that makes the daemon's reading decide
retention, which SRS section 7.4, FR-021 and ADR-0002 forbid. There is no third option the SRS allows.

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
| P9 | **The completion path still hash-verifies every first-party file of the package graph on every popup.** Installed modules are re-checked once per `REVERIFY_TTL` (measured: 2,000 installed modules, about 41 ms per lookup down to about 3 µs, debug build, Windows), but a first-party package's own files are re-read and re-hashed on every completion popup inside its import's braces. That part stays: nothing reports a first-party edit (see the header of `cache/build_memo.rs`), and an equal-length, mtime-preserving rewrite defeats a len+mtime check, so any window would serve a stale export list. |
| P10 | **`ENGINE_PERMITS` is 2, tried at 4 (Task 13), measured, reverted.** Not deferred; see the outcome below. |
| P11 | **An interactive request that joins a prewarm's in-flight analysis waits at prewarm priority.** Prewarm builds hold at most `ENGINE_PERMITS - 1` permits, so a user's own builds never queue behind them. But an import whose cache key a prewarm is already building joins that single-flight build, which may be queued behind the few other prewarm builds already inside the boundary (at most the drain width; foreground work cancels the rest). Bounded and rare (the recent-prewarm job rebuilds only recently used cache keys, of any import kind, that are missing); promoting a flight's priority on join is not worth the mechanism. |

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
