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
things that are still true. When an issue is resolved, cut the entry and add one row to
[Resolved](#resolved); the behaviour belongs in the SRS, the reasoning in the commit, and the guarantee in
the test.

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
| **Resolved** | Fixed. The entry is **deleted** and collapses to one row in [Resolved](#resolved). |

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
**Status: Deferred — RESOLVER scope. The asset half is closed (2026-07-18); the native-addon half is closed (2026-07-19)** · The most visible gap in real projects · Fix universally, never per-package

**The asset half is closed.** A directly imported image, icon or media file was one of these fatal leaves and
is no longer: `import logo from './logo.png'` failed rolldown's loader with `InvalidData` because a PNG is not
UTF-8, and `import mark from './mark.svg'` was worse — an SVG **is** valid UTF-8, so it reached OXC and was
parsed as JavaScript, dying with `PARSE_ERROR: Unexpected JSX expression`. Either one made the whole package
unmeasurable. Both are now classified `AssetClass::Unmeasured`, stubbed to `ModuleType::Empty` like any other
asset, charged against the graph's aggregate byte budget, and **disclosed** at their real size rather than
counted — so the JavaScript measures exactly and the shipped bytes are not silently absent.

The allowlist is deliberately an allowlist. An unknown extension still falls through to Rolldown, because
intercepting something we cannot name would stub a module that might have been real JavaScript.

**The native-addon half is closed too (2026-07-19).** A compiled `.node` addon is now classified
`AssetClass::Unmeasured` alongside an image: stubbed to `ModuleType::Empty` so the JavaScript graph measures,
its bytes charged against the graph budget, and **disclosed** rather than counted — the addon ships as its own
file beside the chunk and is outside the processed taxonomy, so the size omits it and has to say so, exactly
as it does for an image. It earns its place on the interception allowlist by the strongest form of the allowlist's own
argument: Node resolves the extension through `process.dlopen`, so a `.node` file cannot be JavaScript by
definition of its name. The rule is the extension, never the package — `keytar`, `@node-rs/crc32` and every
addon nobody has hit yet are one line in `classify_asset_class`, not three exceptions. Measured after the fix:
`@vscode/vsce` reports 4,636,409 B raw / 448,379 B brotli and `ovsx` 4,783,340 B / 466,764 B, each at Medium
confidence disclosing `keytar.node` at 707,584 B, where both previously reported nothing at all.

**What is left in D6 is two resolver-side classes.** A shipped asset, and then a native addon, each used to be
a trigger for the same symptom; neither contributes to D6 any more. What remains needs the universal
external-boundary fix described below, at the resolver seam:

1. **the unresolvable specifier** — a deep-path `require` (`jest`, `eslint-plugin-autofix`) that poisons the
   whole build;
2. **no importable entry** — a package that declares no `main`/`module`/`exports`/`browser` at all, confirmed
   on `@next/font` on 2026-07-19. The daemon falls back to a literal `index.js` guess and then appends
   extensions to it, so the user is shown a candidate list containing `index.js.js` and `index.js.mjs`, which
   reads as a resolver malfunction when the truth is simply that the package is subpath-only. Two probes for
   this shape already exist on the `entry_resolution` failure branch and both answer with a **labelled
   Measured zero** (`types_only`, and `native_binary_only` for B3), but neither claims this case: a zero would
   be wrong here, because importing such a specifier does not cost nothing, it does not resolve at all. The
   honest answer is a labelled Unmeasured naming the reason — which is what the universal fix below already
   prescribes ("no importable entry"). Note `native_binary_only` ships with no SRS requirement behind it; if
   this class is taken up, charter both in the SRS at the same time.

**What the user sees.** In a real `package.json`, a growing fraction of dependencies render **unavailable**,
and not only native CLIs. The bigger the project, the more of them, which reads as "the build was too big." It
is not a size problem.

**"Unavailable" collapses distinct causes into one word.** Measured against esbuild (Import Lens's own
accuracy oracle) on the installed packages, every failure is fast (4 to 300 ms, never a timeout):

| Package | Real cause (confirmed 2026-07-16) | Class |
| --- | --- | --- |
| `@vscode/vsce` | imports `keytar.node` (a compiled native addon) | native leaf — **fixed 2026-07-19**, measures with the addon disclosed |
| `ovsx` | `keytar.node` plus `@node-rs/crc32`'s `.node` | native leaf — **fixed 2026-07-19**, measures with the addon disclosed |
| `@biomejs/biome` | no importable entry (`bin` only); real tool is a native binary. Now handled by **B3** | native binary (B3) |
| `jest` | `jest-pnp-resolver` does `require('jest-resolve/build/defaultResolver')`, unresolvable, so `[resolve]` fails the whole build | unresolvable leaf |
| `eslint-plugin-autofix` | does `require('eslint/lib/built-in-rules-index')` (eslint's non-exported internals), unresolvable, so `[resolve]` fails the whole build | unresolvable leaf |
| `@next/font` | **confirmed 2026-07-19**: declares no `main`/`module`/`exports`/`browser` at all (only `types`); its real code is subpath-only (`google/`, `local/`), so the root has no importable entry and the fallback guesses `index.js`, then `index.js.js`, `index.js.mjs`, … | no importable entry |

Confirmed 2026-07-16: `jest` and `eslint-plugin-autofix` are not "pure JS that should measure and does not."
Each fails because one transitive `require` targets a deep internal subpath of another package that the
resolver cannot resolve, which is the "one leaf poisons the whole build" case below, not a hidden measurement
bug. `@biomejs/biome` is native-binary-backed and moves to the B3 blocker.

**The universal defect: one leaf poisons the whole number.** A single unbundleable edge (a dynamic `require`,
an unresolvable specifier) anywhere in the graph fails the ENTIRE package build, so a 2 MB
JS graph with one such leaf reports nothing instead of "at least 2 MB, excluding it." Import Lens ALREADY does
the right thing for two classes: it externalizes an unresolvable bare import (`tsdown` measured at 134.7 kB
where esbuild refused on `@tsdown/css`), and since 2026-07-19 it stubs and discloses a `.node` addon. The
remaining gap is that this leniency does not extend to an unresolvable **deep-path** `require` or an
unfollowable dynamic one.

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

### D20: A `.node` specifier that does not resolve is recorded as an unreadable asset input, not an absent one
**Status: Deferred — ENGINE scope. Found by the adversarial review of the `.node` fix (2026-07-19)** · Proven by executed repro against the pinned Rolldown 1.1.5 · No wrong number, no wedge

`ImportLensPlugin::resolve_id` treats any asset-classified specifier whose `ctx.resolve` fails as a **failed
asset input** (`record_failed_asset_input`, `plugin.rs`), which is the right call for a file that exists but
could not be read and the wrong one for a file that was never there. The CSS path already draws that
distinction — `asset_budget.rs`'s `record_failed_path(missing)` selects `absent_file_fingerprint` — and the
plugin has no equivalent. Admitting `.node` to the classifier (2026-07-19) did not create this mechanism; it
gave it a **high-frequency trigger**, which is why it is recorded now and was not before.

**What actually happens, on two paths.** napi-rs generates roughly twenty platform-relative requires per
package (`./crc32.win32-x64-msvc.node`, `./crc32.darwin-arm64.node`, …) inside `try`/`catch`, and ships at
most one of them; Rolldown issues `resolveId` for every one regardless of the `createRequire` reassignment and
the `catch`. So on the **success** path a perfectly good build (`@node-rs/crc32` measured, chunk intact) now
carries ~20 recorded failures, which emits the `asset_io` diagnostic — "supported asset input(s) could not be
read during this analysis; retry after the filesystem settles" — on a successful analysis where nothing needs
retrying, and stamps `unverifiable_asset_fingerprints` (len/mtime `u64::MAX`) into the artifact, so
`fingerprints_are_reusable` is false and every cache refuses the result **permanently**: a full rebuild per
request for the whole napi-rs family. On the **failure** path, a statically imported `.node` that genuinely
does not exist (un-run `node-gyp`, absent platform binary) used to fail `UNRESOLVED_IMPORT` → `resolve`, which
is in `DURABLE_RESULT_STAGES` and cached once; it now short-circuits to `asset_io`, which is deliberately not
durable, so a permanent failure is re-derived on every request under a message calling it transient.

**Why it is not fixed here.** Neither path shows a wrong number — the five sizes stay correct on the success
path and there is no number at all on the failure path — and neither can wedge or lose data. It is a false
transient message plus lost cacheability, which is exactly the "everything else" the fix-now bar defers.

**The fix, when it is taken.** Split absent from unreadable at the plugin's resolve boundary the way the CSS
path already does, and reuse `absent_file_fingerprint` rather than inventing a second ledger — an absent
optional platform binary is a deterministic, cacheable fact about the package, not a filesystem hiccup. Do it
at the boundary so it covers every asset kind at once; do not special-case napi-rs or `.node`.

### D7: A stylesheet its own package declares droppable is counted anyway
**Status: Accepted** · A wrong number on a package shape measured to be absent from the real ecosystem · Found by the B2 adversarial review

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
retention would zero out the CSS for `@uiw/react-md-editor` and undo B2 entirely. The honest fix asks the DECLARATION rather than the build — and that is exactly what this product forbids.
FR-021 (Critical) and the engine boundary contract both state that the daemon's own reading of `sideEffects` is
reporting metadata that "decides a badge, never a byte", and both name rolldown as the only authority on
retention. Dropping an asset on our reading makes it decide bytes. Closing D7 therefore requires amending a
Critical requirement, not writing code, and doing that quietly would be the narrow-the-spec-to-fit-the-code
failure this repository has been bitten by before.

Re-verified 2026-07-18, and the other two blockers are structural rather than incremental. Rolldown's
`HookLoadArgs` carries `id`, `module_idx` and `asserted_module_type` and no importer at all; `resolve_id` has
the importer and discards it on the success path, and the word appears in no daemon file outside `plugin.rs`,
so no asset-to-package mapping exists anywhere to build on. And the `sideEffects` patterns are collapsed to a
bool deliberately — `resolver.rs` says in prose that retaining them "invited a second reading of them", which
is precisely the second reading D7 would need.

Accepted rather than Deferred, because Deferred says "worth doing, not now" and this is not queued work: it is
a measured non-shape in the ecosystem whose fix conflicts with a Critical requirement. Revisit only if the
ecosystem survey changes.

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

A since-deleted plan claimed the provider falls back to `oxc_resolver` for a bare `@import`. It never did, and
resolving CSS with the JavaScript resolver would be worse than not resolving it: that profile has no `style`
main field, no `style` condition and no `.css` extension, so it would answer `pkg/base` with `pkg/base.js` and
measure the wrong file. Doing it properly needs a purpose-built CSS resolver profile.

### D9: A stylesheet's own `@import` tree is bounded at 256 files
**Status: Accepted** · A bound where there was none · Found by the B2 adversarial review

A stylesheet's `@import` children are never graph modules, so none of the engine's limits ever applied to them.
Lightning CSS recurses per `@import`, and a deep enough chain overflows the stack, which is NOT catchable: the
process dies rather than the import failing. One attempt is therefore bounded to 256 files and 8 MB, inside the
AC-03 build-wide 512-read/16 MiB ledger. A production union that consumes the per-attempt limit can exhaust that
shared ledger during retry and end as the typed `module_graph_limit`; the unbounded processor helper still
exercises raw/per-sheet fallback in isolation.

The file count doubles as the depth bound, because a chain of N files costs N reads and nothing else can see
depth from where the bound is applied. 256 stops the walk roughly three times short of where a release build's
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
afterward, so it bounds a tree's total rather than any single file's peak memory. The 20 MB guard on module source
does not cover `@import` children, since they are never graph modules. No real package ships a stylesheet large
enough for that to matter, and a tree that breaches the budget is refused rather than mismeasured, so this is
recorded as a property of the bound rather than treated as a hole in it.

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
floor rather than High with a total. That is the honest reading of what we know, and it is a strict improvement
on the previous behaviour, where those bytes left the headline through a silent `None` while the result still
claimed High confidence.

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
breaching that ledger is **terminal**: `process_stylesheets` turns a live context failure into a hard
`AssetBudgetFailure` for the whole asset stage rather than a graceful degradation.

Worse, the common reason the union fails IS the set breaching a budget together — that is exactly the
shape the regression test at `assets.rs` pins (two sheets, each inside the 256-file per-attempt bound,
breaching it together). In that case the "survivors" are all the sheets, so re-unioning them simply
breaches again, having spent a third of the ledger to learn nothing. And
`may_retry_stylesheets_separately` is a single negative test on the COMPRESSION stage, so there is no
signal distinguishing "one unparseable sheet" from "the set was too large".

The trade would therefore be a **disclosed** over-count (already non-budgetable, already labelled as
reading high) for a possible hard failure of the whole asset stage. The prerequisite is a typed
distinction between a per-sheet parse failure and a set-level budget breach; until that exists, this
is not worth attempting.

### D7 follow-up: per-asset `sideEffects` attribution is further away than recorded
**Status: Still deferred, with a corrected blocker** · Re-examined 2026-07-18

D7's recorded blocker is "needs per-asset package attribution first". That was read as meaning the
attribution was the only missing piece. Re-examination found three separate blockers:

- Rolldown's `load` hook has **no importer parameter**. `args.id` is the asset's own id; the importer
  exists only in `resolve_id`, which discards it. Nothing anywhere maps an asset path back to the
  module that imported it.
- `sideEffects` patterns are **collapsed to a bool at parse time** (`SideEffectsMode::Array { entry_matches }`).
  The patterns are deliberately not retained, so the value cannot be re-asked about a different path.
- The engine boundary contract states the daemon's own reading of `sideEffects` is "reporting
  metadata — it decides a badge, never a byte". Dropping an asset on that reading makes it decide
  bytes, which is the thing the contract exists to prevent.

Adding asset paths to the contribution model (D15) did **not** unblock this, contrary to an earlier
note in the 2026-07-18 review.

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
for zero-byte operations would be more machinery than the risk justifies. The per-attempt snapshot
copy that made retries quadratic **was** fixed: the ledger is now consulted one path at a time
instead of being cloned per attempt.

### D2: An honest lower bound on a failed build
**Status: Deferred** · The intended successor to ADR-0003

Today an unbuildable import reports no size. A graph-limit breach means much of the graph was loaded before we
stopped, so a real floor exists: "at least 4 MB; graph limit exceeded" is strictly better than a blank. The
engine currently discards the partial graph on failure, so this needs plumbing through the engine boundary.

### D4: A file with one unmeasurable import can never cache its total
**Status: Deferred** · A performance cost of an invariant we want

An aggregate missing a contributor's bytes is a **floor**, and a floor is never cached. So a file containing
one permanently-broken import, or one deterministically unprocessable supported asset, re-runs its combined
build and asset tail on every size request. The per-import deterministic outcome is still cached; the file
aggregate cannot be, because it is not a complete File Cost.

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

# Resolved

One line each, and deliberately no more. A fixed issue is not a known issue, and a tracker that keeps
every fix forever stops being read. The detail lives where it belongs: the behaviour in the SRS, the
reasoning in the commit that made the change, the guarantee in the test that holds it. What survives here is
only the identifier, because code comments and older entries refer to these by name and a reference that
resolves to nothing is worse than the bloat.

| ID | What it was | Fixed |
| --- | --- | --- |
| D11 | An asset the daemon could not read is disclosed, and that disclosure is cached | 2026-07-18 |
| D12 | A file total that omits an asset is not structurally flagged as a floor | 2026-07-18 |
| D20 | Nested rayon inside the asset pool does not widen its admission | 2026-07-18 |
| D21 | A missing @import target made the whole asset result permanently non-durable | 2026-07-18 |
| D10 | Every reported brotli size was high, because the daemon compressed at quality 4 | 2026-07-18 |
| D15 | `shared_bytes` explains JavaScript sharing only | 2026-07-18 |
| D16 | Asset composition reaches only the hover | 2026-07-18 |
| D17 | A per-import floor can still be compared against a budget | 2026-07-18 |
| B2 | The Import Cost ignored shipped non-JS asset bytes (CSS, wasm, fonts) | 2026-07-17 |
| B1 | An all-inline-`type` named import was measured as the whole package | 2026-07-16 |
| B3 | Native-binary-backed packages were mismeasured instead of labelled | 2026-07-16 |
| K1 | The `sideEffects` badge fix is invisible on a warm cache until `ANALYZER_REVISION` moves | 2026-07-15 |
| U1 | `a6cae06` did not get an adversarial review | 2026-07-16 |
