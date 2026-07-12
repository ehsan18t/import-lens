# Bundler Redesign — Independent Release Review

> **Superseded, 2026-07-12 (same day), by
> [`2026-07-12-bundler-redesign-release-plan.md`](2026-07-12-bundler-redesign-release-plan.md).**
> This document is retained as the *findings* record. It is no longer the plan, and four things in it
> are now wrong:
>
> - **§7's sequence is superseded.** The plan orders the work by dependency and puts the instruments
>   first.
> - **EXT-1's recommended fix is rejected.** "The honest total requires deduplication by module" would
>   build a project-level bundle model. Import Lens measures **imports, not bundles**
>   ([ADR-0004](../../adr/0004-import-lens-measures-imports-not-bundles.md)) — that was never the
>   product, not a deferred one. The review's *fallback* option is what ships: the figure is relabelled
>   **Combined Import Cost**, and the arithmetic stands.
> - **SF-11 is half wrong.** It claims "the protocol has no runtime to pass".
>   `CompleteImportMembersRequest` already carries `source` and `cursor_offset`, and the daemon already
>   classifies a document offset into a runtime — so that path needs **no protocol change**. Only
>   `EnumerateExports` lacks the input.
> - **§6b.5 (uncounted non-JS bytes) is not a post-release idea; it is a release floor.** Adopting
>   ADR-0003 turns every CSS-shipping package from "wrong number" into "blank row", because the adapter
>   *fails* any build emitting an asset. See below.
>
> Two defects the review missed, both found by the interview:
>
> 1. **A CSS-importing package cannot be built at all** — `adapter.rs` demands "one chunk and no
>    assets", and a `.css` module becomes an asset. Masked today by the very fallback ADR-0003 deletes.
>    No pinned fixture ships CSS, which is why qualification never saw it.
> 2. **The side-effects glob matcher is hand-rolled** (~80 lines; no glob crate in `Cargo.toml`) while
>    `fast-glob` — Rolldown's own matcher — is already in `Cargo.lock`. Harmless only because
>    `|| is_array()` discarded its answer; SF-3's fix makes it load-bearing for a user-facing badge.

Status: **findings, not yet actioned**. Reviewed 2026-07-12 against
`docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` (the spec of record) and the
shipped code on `bundler-redesign`. Plan documents were deliberately not read: this review
asks what the code does, not what a plan says it should do.

Every finding below was reproduced against the source before being written down. Claims that did
**not** survive that check are recorded in §5, so nobody re-litigates them.

**Revision 2 (2026-07-12, same day).** The review was re-audited against itself: every finding I
had not personally reproduced was handed to an adversarial verifier told to refute it, and the
spec was re-walked clause by clause for anything the first pass missed. Results:

- **SF-6 was a false positive** and is withdrawn (§5). The blocking work it complained about is
  memoized and lives on the right runtime.
- **SF-4, SF-5 and SF-7 survived** refutation with their evidence strengthened — in SF-7's case by
  the rolldown 1.1.5 registry manifest, which confirms every workspace sibling is a *caret* range,
  so the drift it describes is not merely possible but inevitable on the next upstream patch.
- **Four new should-fix findings** (SF-9 … SF-12) and **four new improvements** were missed by the
  first pass. Three of them — SF-9, SF-10, SF-11 — are clustered in the file-size and enumeration
  paths, which the first sweep under-covered because it followed the *engine* rather than the
  *product surfaces built on it*. That is the honest lesson of the second pass.
- `pnpm test` and `pnpm test:accuracy` both run green today (`css-tree` worst case 13.0% against
  the 25% tolerance), independently confirming the §10.7 record.

## 1. Verdict

The cutover is real and it is good. The old engine is genuinely gone, Rolldown is the sole
semantic authority, the adapter leaks no Rolldown type, the four §2.2 defects are fixed, and
the daemon is roughly **1.7× faster per cold build** than the engine it replaced. The
architecture the spec asked for is the architecture that shipped.

It is **not release-ready**, for three reasons that are all about the *edges*, not the design:

1. a panic inside a build takes down the whole request instead of degrading one import;
2. the performance and memory gates that are supposed to protect this engine **never run**;
3. the measured package's own `package.json` never reaches Rolldown, and the test matrix
   cannot see that because every side-effects row measures a fixture shaped unlike production.

None of these require redesign. They are bounded fixes.

## 2. What is better than before

Recorded because a review that only lists defects misrepresents the work.

- **Correctness the old engine could not reach.** `css-tree/parse` emits **zero** dangling
  `__il_` bindings (was 15, then 4 after a five-commit fix campaign); `date-fns/format` stays
  at zero. All four §2.2 defects — escaping empty namespace, effectful unused initializer,
  ambiguous `export *`, external re-export emitting a zero-byte bundle — are fixed and pinned
  by matrix rows.
- **Faster, not just more correct.** Cold `css-tree/parse` p95 is 52 ms against a 500 ms gate;
  20-import batch peak RSS is 78 MB against a 400 MB gate. The engine-runtime sizing fix
  (`boundary.rs:41-45`, `available_parallelism` clamped to `[2,8]` instead of a 2-thread
  runtime) moved the real-package cold median from 299 ms to 181 ms.
- **A staleness race the old engine had is now structurally closed.** Module bytes are
  fingerprinted *in the load hook at read time* (`plugin.rs:245-274`), with the stat taken
  before the read. A file edited during the analysis window can no longer be recorded with new
  bytes against a size measured from old ones — the failure mode where an entry never
  self-heals. This is better than what the spec asked for.
- **Honest reporting under uncertainty.** `matched_side_effect_paths` is empty and a
  conservative diagnostic is emitted rather than a re-implemented glob matcher
  (`adapter.rs:214-236`), and the retained matcher is quarantined to the static-fallback path
  exactly as the I9 amendment promises (`resolver.rs:44` has exactly one caller,
  `analyze.rs:514`).
- **The dependency story is airtight.** Exact pins on all 11 coordinated crates, a 52-package
  fingerprint checked against `cargo metadata --locked`, `--locked` on every non-update cargo
  entry point, and a Guard test that fails if `rolldown` ever becomes optional again.
- **An anti-vacuity Guard.** `dangling_binding_gate_is_not_vacuous`
  (`candidate_packages.rs:194`) proves the suite's central assertion can actually fire. That is
  a higher standard than the spec demanded.

## 3. Release blockers

### RB-1 — A panic inside a build destroys the whole request, not one import

**Spec:** §12 ("No failure path may … silently switch to an unvalidated result"; every build
failure is a typed stage plus conservative fallback). **Evidence:** `daemon/src/engine/boundary.rs:81-89`.

`run_on_engine` spawns the build on the engine runtime and blocks on
`receiver.recv().expect("the engine runtime should always reply")`. There is no `catch_unwind`
anywhere in `daemon/src/engine/`. If a Rolldown or OXC build panics, Tokio catches the task
panic and drops the sender, `recv()` returns `Err`, and **the calling analysis thread panics**
— with a message blaming the runtime. The panic propagates out of `thread::scope`
(`scheduling.rs:34-50`) → `drain_classified` → `handle_batch`, and
`ipc/server.rs:1038-1041` converts the `JoinError` into a generic batch-level protocol error.

Net effect: one pathological package turns an entire batch — **including every import already
answered from cache** — into a single "analysis worker failed" error, instead of that one
import degrading to a static fallback with a stage diagnostic.

Making the release profile unwind (`6707baf`) was necessary but not sufficient: it means the
daemon *survives*, and the report path is protected (`service.rs:563`), but the interactive
paths — `Batch`, `AnalyzeDocument`, `FileSize`, `AnalyzePackageJson` — have **zero** engine
panic isolation. The one place a panic is most likely is the one place it is not caught.

**Fix:** wrap the build future in `AssertUnwindSafe(...).catch_unwind()` inside `with_permit`,
map to `BundleFailure { stage: "panic", .. }`, and let the existing §12 fallback arm
(`analyze.rs:258-263`) do its job.

**Companion defect:** `IN_FLIGHT.fetch_sub` (`boundary.rs:71`) is skipped on unwind, so the
counter leaks permanently and `PEAK_IN_FLIGHT` latches an inflated value. The §9 two-build
invariant is asserted *only* through `peak_in_flight()` (`engine_boundary.rs:69`), so after two
panicking builds the daemon's sole concurrency check reports garbage. Use a drop-guard, as the
semaphore permit already does.

### RB-2 — The §10.6 performance and memory gates never run

**Spec:** §10.6, §4.6 step 5 ("a Rolldown upgrade must rerun the complete … absolute
performance, memory, and concurrency gates"), §15. **Evidence:**
`daemon/tests/candidate_performance.rs:92,130` — both tests are `#[ignore]`d and require
`IMPORT_LENS_FIXTURES_WORKSPACE`. A repo-wide grep for `candidate_performance` outside the plan
docs returns the file's own doc comment, `tests/common/mod.rs:69`,
`prepare-candidate-fixtures.mjs:5`, and the upgrade skill — **no workflow step and no
`package.json` script ever invokes it.**

The trap is that a perf gate *appears* to run: `validate.yml:150` calls `pnpm test:performance`,
which is `package.json:221` → `cargo test … --test performance` — the pre-existing legacy suite
over synthetic fixtures, a different file entirely. And `run_performance` is only `true` in
`build.yml`'s `workflow_dispatch`, so even that never fires on a PR.

**Failure scenario:** a Rolldown 1.1.5 → 1.1.6 bump doubles cold p95 or blows the 20-import RSS
ceiling on `css-tree`/`date-fns`. `deps:update:compiler` succeeds, the fingerprint updates,
`candidate_packages` still passes (it asserts correctness, not timing), CI is green — and the
regression ships. The suite written precisely to prevent this has never run in CI.

This matters more than a normal missing test because the owner's stated target is *the most
stable and best-performing app*, and §10.6 is the only thing that defends it. Contrast with
`candidate_packages.rs`: also `#[ignore]`d, but genuinely wired up
(`validate.yml:124-144` installs fixtures and runs `-- --ignored`). The pattern exists; the
perf suite just was not connected to it.

**Fix:** add a `candidate_performance` step alongside the existing `candidate_packages` step in
`validate.yml`, reusing the same fixture install. Gate the assertions on the §10.6 absolute
numbers, not on a comparison against a now-deleted engine.

### RB-3 — Packaging, daemon-hash refresh, and the VSIX size check are still deferred

**Spec:** status header (lines 8-11) and §15. The daemon binary changed; these were deferred by
owner direction on 2026-07-11 and are explicitly required before any release. Not a defect —
just the outstanding gate. `pnpm package:win32-x64` plus the hash refresh must run and be
committed.

## 4. Should-fix before release

### SF-1 — The measured package's `package.json` never reaches Rolldown

**Spec:** §7.4 (Rolldown "uses its native resolver's nearest-package metadata, built-in
`package.json#sideEffects` … handling"). **Evidence:** `plugin.rs:189-191` returns
`HookResolveIdOutput::from_id(target)`. That type carries a `package_json_path: Option<String>`
field (verified in the vendored `rolldown_plugin-1.1.5` source), and for a plugin-resolved id
Rolldown builds `ResolvedId.package_json` **only** from that field. The entry therefore gets
`package_json: None`, and its side-effect classification falls back to pure source analysis.

Every *transitive* module is resolved by Rolldown itself and does get its package metadata — so
the entry module is the sole hole. But the entry module is *the file every measurement is
rooted at*.

**Blast radius, stated honestly:** the entry module is always retained (it provides the
requested export), so this cannot drop a needed module. What it changes is *statement* retention
**within the entry file**: a package declaring `"sideEffects": false` whose entry has top-level
statements that source analysis cannot prove pure (`Object.freeze(...)`, a prototype patch, a
self-registration call, an unannotated factory invocation) keeps those statements and everything
they reach. Rollup and webpack would drop them. The reported size is inflated. For a pure
re-export barrel — which most of the pinned fixtures are — the effect is nil, which is exactly
why accuracy still sits at 13% against esbuild and why nothing caught this.

`BundleEntry.package_root` is already carried (`engine/mod.rs:32`) and currently used only as
`cwd`. The fix is to supply `package_json_path = package_root/package.json` — **metadata supply,
not a semantic override**, so it is squarely allowed by §7.4 and §14.6.

### SF-2 — The side-effects matrix rows cannot see SF-1, because they don't test production's shape

**Spec:** §10.2, §10.4. **Evidence:** `candidate_matrix.rs:950-961` — every one of rows 38-44
builds its fixture with `write_side_effect_package`, which writes a **workspace-root `entry.js`**
that does `import 'testpkg'`. So `testpkg` is a *transitive* dependency resolved by Rolldown
(and correctly gets its `package.json`), while the measured entry is a bare workspace file
belonging to no package at all.

Production is the opposite shape: the user imports `date-fns`, so `entry_path` **is**
`node_modules/date-fns/…`, resolved by the plugin — the exact path that loses its metadata. The
seven rows that exist to prove "Rolldown owns `sideEffects`" all exercise the one code path
production never takes.

This is the most important structural finding in the review. The matrix is otherwise strong; this
row family is measuring the wrong thing, and it is what let SF-1 stay invisible.

**Fix:** add a row whose `BundleEntry` points *into* a `node_modules` package that declares
`"sideEffects": false` and whose entry file carries an impure-looking top-level statement, and
assert the statement is dropped. That row fails today and passes after SF-1.

### SF-3 — Any array `sideEffects` unconditionally reports `side_effects: true` and kills `truly_treeshakeable`

**Spec:** §7.4 ("Missing reporting metadata yields a conservative confidence **diagnostic, not a
semantic override**"). **Evidence:** `analyze.rs:318`:

```rust
let side_effects = side_effects_mode.has_side_effects() || side_effects_mode.is_array();
```

`has_side_effects()` **already answers correctly for arrays** — it consults the matched patterns
computed by `side_effects_array_mode` (`resolver.rs:36-41`, `:696-722`). The `|| is_array()`
overrides that correct answer with `true`, unconditionally. Because `analyze.rs:339` gates the
whole full-package comparison on `!side_effects`, `truly_treeshakeable` is then `false`
**by construction** — the comparison build never even runs — and `adapter.rs:214-236` pushes a
diagnostic that drops `engine_confidence` to `Medium`.

`"sideEffects": ["**/*.css"]` is an extremely common declaration. Every such package is reported
side-effectful, never truly tree-shakeable, and never high-confidence — even when Rolldown
demonstrably tree-shook it and the measured size proves it. `report/model.rs:91` then raises a
warning on every such row.

The code's own comment justifies this with "glob matching unavailable from public bundler
metadata" — but the **2026-07-12 retraction in the spec (§10.7, divergence 1) refutes exactly
that premise**, and matrix rows 42/43 now *prove* Rolldown matches string and array globs
correctly on Windows. The justification was retracted; the conservatism it justified was not.

**Fix:** drop `|| side_effects_mode.is_array()`. Decide explicitly what the array form should
report (entry-matches is already computed and is a defensible proxy) and pin it with a test —
`analyze.rs` tests currently cover only the string form (`daemon/tests/analyze.rs:2009-2046`).

### SF-4 — Failure stage is chosen by diagnostic vector order (determinism gate)

**Spec:** §10.6 ("repeated identical runs produce deterministic … failure stages").
**Evidence:** `adapter.rs:297-301` — the reported stage is the *first non-`link` diagnostic in
Rolldown's vector*, and Rolldown accumulates diagnostics from module tasks running concurrently
on the engine runtime. A build with a parse error in module A and an unresolved import in module
B can report `stage: "parse"` on one run and `"resolve"` on the next, for identical inputs. The
stage is user-visible **and cached**. Rank stages by a fixed priority instead.

### SF-5 — Export-enumeration memo ignores `package.json`, so it can serve a stale export list forever

**Spec:** §8.3 ("package manifests used for resolution or side-effect classification are included
alongside source paths"), §5 ("expire exactly when the files it was derived from change").
**Evidence:** `export_list.rs:37-45` stores only `enumeration.read_time_fingerprints` (source
modules). The size path deliberately adds the root and first-party manifests
(`analyze.rs:410-412`); enumeration does not. The memo has no TTL.

**Failure scenario:** a first-party/workspace package under development. The user flips
`"type": "module"` (or edits `exports`/`sideEffects`) in its `package.json`. No source file
moves, so no fingerprint changes and no cache generation bumps — and the completion popup serves
the **old export list indefinitely**.

### SF-6 — **WITHDRAWN.** False positive; see §5.

Claimed that blocking `canonicalize` + source hashing on the engine's async workers violated §9.
It does not. Retained as a numbered hole so the other identifiers stay stable.

### SF-9 — File sizing builds one bundle *per runtime*, which §6.3 forbids, and no amendment records it

**Spec:** §6.3 ("`file_size.rs` supplies all resolved requests in **one** `BundleRequest` … the
adapter **must not concatenate** independently generated package bundles") and the §3.1 goal
"keep **one** in-memory build for file-level multi-import sizing".
**Evidence:** `file_size.rs:80` groups imports into a `BTreeMap<ImportRuntime, RuntimeGroup>`,
`:117-122` issues one `bundle_sync` per group, and `:210-222` joins the groups' **minified**
outputs and compresses the concatenation once.

The behavior is almost certainly *right* — Server and Client resolve dependencies under
materially different conditions, and a single Astro file legitimately mixes both, so one shared
chunk would be incorrect. That is not the finding. The finding is that **the spec of record still
says the opposite**, and the rationale for the deviation lives only in a code comment and a
downstream SRS line. The design doc amended itself for the retained side-effect matcher (I9); it
never did so here.

Two concrete consequences beyond the paperwork:

- The compressed totals (`gzip`/`brotli`/`zstd`) are computed once over the *concatenation* of
  both runtime groups, so redundancy between the two is compressed away exactly once. The
  reported compressed size is therefore a strict **lower bound** on what the two bundles that
  actually ship would compress to — under-reported, with no diagnostic.
- It is the direct cause of SF-10.

### SF-10 — Cross-runtime modules are reported as "shared" even though each runtime ships its own copy

**Spec:** the §3.1/§6.3 model ("shared dependencies are counted once") as actually implemented —
a package imported under two runtimes is genuinely counted once *per runtime*, because each
runtime really does ship its own copy.
**Evidence:** `file_size.rs:27-46` — `annotate_shared_bytes` counts each module path's occurrences
across **every** `ImportResult` in the document, with **no runtime partition**. The extension then
renders that as a savings insight (`extension/src/analysis/insights.ts:112-137`).

**Failure scenario:** an Astro file imports the same package from frontmatter (Server) and from a
client script (Client). The common modules appear in both results, so `shared_bytes` is non-zero
and the UI tells the user "shared dependency — also appears in …", claiming a deduplication that
the per-runtime build model explicitly does not perform. The user is shown a saving that does not
exist, on exactly the file shape the runtime split was introduced to handle correctly.

### SF-11 — Export enumeration is hardcoded to the browser runtime, so server-file completions can be wrong

**Spec:** §8.4, and §6.1/§7.1 (root resolution and Rolldown resolve options are per-runtime).
**Evidence:** `service.rs:1614` builds the enumeration request with `runtime:
ImportRuntime::Component` — unconditionally. The protocol has no runtime to pass:
`EnumerateExportsRequest` (`ipc/protocol.rs:686-698`) and `CompleteImportMembersRequest`
(`:611-621`) carry no runtime field. Component/Client resolve with `alias_fields = ["browser"]`
and browser conditions; Server resolves with node conditions (`resolver.rs:249,578`).

**Failure scenario:** in a Server-context file (Astro frontmatter is Server for sizing), a package
whose `exports` map exposes different surfaces under `node` and `browser` is enumerated under
**browser** conditions. The completion list omits names the file can actually import and offers
names it cannot — while the *size* analysis of that same file correctly uses the Server runtime.
Completions and sizing disagree by construction. Secondary: the enumeration memo is keyed by
runtime, but production only ever writes the `Component` key, so that dimension is dead.

### SF-12 — The `truly_treeshakeable` re-baseline required by Phase 4 has no instrument and never ran

**Spec:** §11 Phase 4 ("Re-run real-package accuracy **and `truly_treeshakeable`** baselines") and
§15.
**Evidence:** `scripts/accuracy-compare.mjs` exists and is green. **Nothing in `scripts/` mentions
`truly_treeshakeable` at all.** The only assertions are synthetic unit fixtures in
`daemon/tests/analyze.rs`; `candidate_packages.rs` works at engine level and never produces an
`ImportResult`, so no real package's badge is baselined anywhere.

The design's status header claims Phase 4's accuracy half is green and is simply silent on this
half. So the flag that moved most visibly at cutover — and that SF-3 shows is forced to `false`
for any array `sideEffects` — has no real-package ground truth, and a regression in it ships
undetected.

### SF-7 — `deps:update:safe` under-restores the compiler stack

**Spec:** §4.4 (it must restore "every compiler-stack package to the exact package/version set
recorded by the compiler-stack configuration"). **Evidence:** `deps-update-safe.mjs:41-48`
builds its restore pins from the direct crates only — 11 packages — while the recorded set
(`scripts/compiler-stack.fingerprint.json`) is **52**. Its own comment (lines 21-23) concedes
that Rolldown's caret ranges let a general update move its workspace crates; the restore loop
then never touches them.

**Failure scenario:** `cargo update` moves `rolldown_utils`/`rolldown_plugin_*` within their
caret ranges while `rolldown` itself stays exact. The restore fixes the 11, the fingerprint still
mismatches, and the command **fails for a case §4.4 says it should have restored** — leaving a
mutated `Cargo.lock` and `pnpm-lock.yaml` with no recovery path but `git checkout`. Derive the
`--precise` restore loop from the fingerprint's package list.

### SF-8 — SRS states an analyzer revision and an architecture the code contradicts

**Spec:** §15 ("the SRS reflects the accepted architecture"). Three lines are false today:

- `ImportLens-SRS.md:1325` and `:1450` both say `ANALYZER_REVISION` moved to **`rolldown1`**.
  The shipped constant is **`rolldown2`** (`cache/key.rs:43-49`). *(The design doc's own status
  header, line 6, is stale the same way.)*
- `ImportLens-SRS.md:1325` says the daemon "does not implement … side-effect glob matching". It
  does — `resolver.rs:44`, deliberately **retained** by the I9 amendment. The SRS contradicts the
  amendment it exists to record.
- `ImportLens-SRS.md:1668` still lists "the Windows `sideEffects`-glob matching defect (FR-021)
  being fixed" as an upstream trigger to watch. That defect was **refuted** on 2026-07-12. The
  SRS is watching for a fix to a bug that does not exist.

The concrete risk is not pedantic: someone reading line 1325 deletes `matching_paths` as dead
code (the SRS says the daemon does not do glob matching), silently degrading the static-fallback
`side_effects` flag.

## 4b. Revision 3 — the aggregates are not sound (found by auditing the surfaces, not the engine)

Every prior pass audited the daemon against the spec. None audited **what the product does with the
numbers afterwards**. Doing that surfaced a defect class that the spec never covers, because the
spec stops at `ImportResult`: *the aggregate figures are computed by summing per-import compressed
bytes.* That is wrong twice over, and both errors push the same direction — **over-count**.

1. **Shared dependencies are counted once per import site.** Fifty files importing `react` count
   React fifty times. The bundle ships it once.
2. **Compressed bytes are not additive.** Brotli of `A` plus Brotli of `B` is strictly greater than
   Brotli of `A ∪ B` — compression finds redundancy across the union that it cannot see when the
   parts are compressed separately. Summing compressed sizes is not a valid operation *even when
   nothing is shared*.

### EXT-1 — The workspace report's headline "Total Brotli" is a sum of per-import brotli bytes

**Severity: BLOCKER** (for the workspace-report feature; it is the flagship number and it is not a
real quantity).
**Evidence:** `daemon/src/report/model.rs:71` — `total_brotli_bytes = rows.iter().map(|row|
row.brotli_bytes).sum()`, one row per import. Rendered as the report's top metric,
`extension/src/ui/report.ts:137`: `<div class="metric">Total Brotli<strong>…`.

A developer reads "Total Brotli" on a workspace report as *"this is what my project ships."* It is
not. For a real application it can overstate by a large multiple — every shared framework, utility
and polyfill is re-counted at every import site, then the compressed sizes are added as though
compression were linear.

The irony is that the report **already knows**: it builds `duplicate_imports` and `shared_modules`
groups (`model.rs:80-81`) — it identifies the very duplication it then refuses to subtract from the
total.

Two derived figures inherit the error:

- **Treemap percentages** (`model.rs:354`) use the inflated sum as their denominator, so every
  slice is a share of a fictitious quantity.
- **The duplicate-imports table** (`model.rs:274`, `report.ts:73`) reports a per-specifier "Total
  Brotli" that is `count × per-import cost` — so `react`, imported in fifty files, is presented as
  costing fifty Reacts.

**Fix shape:** the honest total requires deduplication by module, not by row — the per-module
contributions are already on each result. Failing that, the number must be relabelled to what it
actually is (a sum of independent import costs, useful for ranking, meaningless as a bundle size),
and the treemap denominator with it. Do not ship a figure called "Total" that is not one.

### EXT-2 — The per-file budget sums per-import brotli, while the correct deduplicated total is already on hand

**Severity: SHOULD-FIX** (produces false "budget exceeded" diagnostics in the editor).
**Evidence:** `extension/src/analysis/budgets.ts:67-99` — `fileBrotliBytes += actualBytes` over each
import's `brotli_bytes`, then compared against `perFileBrotliBytes`.

This one is worse than EXT-1 because the right number **already exists and is already fetched**. The
daemon's file-size path builds all of a document's imports in *one* bundle precisely so shared
modules are counted once (that is what §6.3 exists for), and `listener.ts:206-249` already requests
it and displays it in the status bar. The budget check ignores that result and re-derives a worse
one by addition.

**Failure scenario:** a file with five imports from one framework — say five `@mui/material`
subpath imports sharing most of their graph. The status bar (correctly) shows the deduplicated
total; the budget checker sums the five and can easily land at 2–3× that, raising a "file budget
exceeded" diagnostic on a file that is inside its budget. The user is shown two different totals for
the same file, and the wrong one is the one that produces the warning.

**Fix:** feed the budget check the `FileSizeDocument` result the controller already has.

### EXT-3 — The shared-dependency tooltip indexes by specifier; the daemon computes shared bytes by result

**Severity: IMPROVEMENT.**
**Evidence:** `extension/src/analysis/insights.ts:177-193` — `sharedModuleIndex` maps a module path
to the set of **specifiers** that contain it, and a module counts as shared only when that set
exceeds one. The daemon's `shared_bytes` (`daemon/src/pipeline/file_size.rs:27-46`) counts a module
as shared when it appears in more than one **result**.

Those disagree whenever one specifier produces two results — which is the extremely common
`import React, { useState } from "react"` (a default import *and* a named import, two results, one
specifier). The daemon reports non-zero `shared_bytes`; the extension's index sees a single
specifier, finds no shared module to name, and falls through to the tooltip at `insights.ts:117-119`
telling the user the shared bytes are *"outside the public top-module breakdown"* — which is false.
They are inside it; they are just shared with the other half of the same import statement.

## 5. Claims raised and refuted

Recorded so they are not re-raised. Each was checked against the code, not reasoned about.

- **"Unresolved imports fail the build and force a static fallback."** False. `stage_for` maps
  `UnresolvedImport → "resolve"` (`adapter.rs:337-340`), but that mapping only applies to the
  *error* vector. Rolldown reports an unresolved import as a **warning** and externalizes it —
  matrix row 25 (`candidate_matrix.rs:600-618`) calls `bundle_ok` on an unresolvable package and
  asserts the boundary survives in the output. The build succeeds, exactly as §10.7 divergence 4
  records. *(A real but minor defect hides here: `warning_diagnostics` stamps **every** warning
  with `stage: "generate"` (`adapter.rs:358-366`), so an unresolved-import warning is labelled
  `generate` rather than `resolve`/`external`. Cosmetic, worth a one-line fix.)*
- **"`ENGINE_PERMITS` = 2 was measured on a 2-thread runtime and is now wrong."** Half true, and
  already handled: the *runtime width* bug is fixed (`boundary.rs:41-45`); the permit count is
  deliberately still 2 because permits bound **peak memory**, not speed. Raising it is a real
  opportunity (§6) but not a defect.
- **"The success path can return a partial graph after a limit breach."** Not reachable today —
  Rolldown propagates the hook error with `?`, so a breach always fails the build. Worth a
  one-line guard in `translate` as defense-in-depth, nothing more.
- **SF-6: "Blocking `canonicalize` and source hashing park the engine's async workers." WITHDRAWN
  on re-verification — this was a false positive of my own.** Three of its four load-bearing
  claims are wrong. (1) `canonicalize` is **memoized per build** (`plugin.rs:85-104`): each
  distinct path is resolved once, and the lock is deliberately *not* held across the syscall. The
  "2,000 blocking syscalls" picture read the memo's cold path as if it were the hot path. (2) The
  bulk I/O in the same hook already uses `tokio::fs::metadata` and `tokio::fs::read`, which go to
  the blocking pool, not the engine workers. (3) §9's rule is about the **Tokio I/O threads**; the
  engine runtime is a *separate* runtime, and those workers are precisely where Rolldown reads,
  parses and transforms modules by design — an xxh3 hash there is a rounding error against the
  parse of the same bytes on the same thread. Removing the read-time hash to "fix" this would
  reintroduce the staleness race it exists to close. **Residue:** `std::fs::canonicalize` on an
  async worker is a hygiene nit worth one line (`tokio::fs::canonicalize`), nothing more —
  demoted to improvement 16.

## 6. Improvements (post-release, ordered by value)

1. **Raise `ENGINE_PERMITS` from 2 to `available_parallelism().clamp(2, 4)`.** The 20-import
   batch measures **78 MB peak against a 400 MB gate** — a 5× memory headroom — while 20 misses
   serialize into 10 sequential rounds. §10.7 explicitly authorizes "one bounded optimization
   pass [to] adjust … the build-concurrency limit". Best available throughput win.
2. **Fix the prewarm priority inversion.** Prefetch drains through the *same* global semaphore
   (`prefetch.rs:338,380`), which is FIFO-fair, and `cancel()` only stops jobs not yet started.
   A user typing an import can queue behind two in-progress prewarm builds and pay their full
   remaining time (~300 ms tail) before starting. Reserve an interactive permit.
3. **Answer `CacheProbe::Unresolved` in the classify pass.** `service.rs:664,2068` route every
   non-hit — including types-only, node-builtin, and unresolvable imports, none of which
   construct a bundler — through the 4-wide engine drain instead of the Rayon pool.
4. **Drop the per-module source clone.** `plugin.rs:257` does `String::from_utf8(bytes.clone())`
   solely because `content_hash(&bytes)` is used afterwards. Hash first, then move the buffer:
   one full extra copy of the entire graph source (up to 100 MiB by the limit, ×2 permits) per
   build, for nothing.
5. **Avoid copying the linked chunk.** `adapter.rs:241` `chunk.code.clone()` copies a
   multi-megabyte string just to move it into `BundleArtifact`.
6. **LRU the dependency-path index and raise its bound.** `dependency_paths.rs:15,57-63` caps at
   32 entries and evicts an **arbitrary** `HashMap` victim. Above 32 `(entry, runtime)` pairs, a
   monorepo thrashes it, and the loser's first-party file-size freshness silently degrades to an
   entry-only stat (`file_size_cache.rs:214-218`) — nondeterministically across runs.
7. **`drain_ordered` uses 2 workers where `drain_classified` deliberately uses 4.**
   (`scheduling.rs:63` vs `:20`.) The post-build tail (minify + 3 compressors + fingerprint +
   cache insert) runs *after* the permit is released, which is why the wider count exists — but
   package.json analysis and both prefetch drains use the narrow variant, so both permits idle
   with work queued.
8. **Surface the full-package minify failure.** `analyze.rs:367` — `.ok()?` silently degrades
   `truly_treeshakeable` to `false` with no diagnostic, while the sibling build-failure branch
   does push one.
9. **Add a Guard test for `--locked`.** Every cargo invocation is correct today, but nothing
   asserts it. A textbook Guard (assert an anti-pattern is absent) and policy-compliant.
10. **Tighten four weak matrix rows.** `matrix_12`/`matrix_13` (namespace optional reads,
    escaping namespaces) assert only that the artifact is valid — a regression that materialized
    an escaping namespace as `{}` would keep them green. `matrix_45` claims to cover "semantic
    failures" but asserts `stage == "parse"`, which `matrix_31` already pins. `matrix_35` never
    asserts single-chunk output. Also missing: a plain `export { v as w }` alias row and the
    string-literal **import** form.
11. **Reduce the IPC runtime to 2 workers.** `main.rs:12` `#[tokio::main]` spawns `num_cpus`
    workers for pure named-pipe framing; all real work is already `spawn_blocking`. Cheap startup
    and idle-RSS win against the §10.6 gates.
12. **`#[serde(skip)]` on `internal_contributions`** (`ipc/protocol.rs:215`) makes `shared_bytes`
    fall back to the top-10 `module_breakdown` on L2 disk hits (`file_size.rs:48-54`) — the same
    file reports different shared bytes depending on whether the result came from memory or disk.
13. **Minor doc drift.** `.claude/skills/compiler-stack-upgrade/references/sources-and-surface.md:5`
    still calls rolldown the "candidate" crate, contradicting `SKILL.md:32`.
14. **`BundlePurpose` is a write-only field on the §5 contract.** Declared in `engine/mod.rs:24,46-51`
    and constructed at four call sites; **zero readers** anywhere in the engine or the pipeline. It
    reads as though the engine varies behavior by purpose — it does not. Either read it or delete
    it; as written it is a trap for the next person tuning the full-package path.
15. **`contributions` and `loaded_paths` spell the same file differently.** `loaded_paths` are
    canonicalized (Windows verbatim `\\?\C:\…`); contributions carry Rolldown's raw module id.
    Nothing joins across them today, so nothing is broken — but the qualification suite already has
    to bridge them by hand (`candidate_packages.rs:74-78`), and any future join against fingerprint
    paths would silently match nothing on Windows. Normalize at the adapter boundary.
16. **`std::fs::canonicalize` on an engine worker** (`plugin.rs:98`) — use `tokio::fs::canonicalize`.
    This is the entire surviving residue of the withdrawn SF-6; it is hygiene, not a defect.
17. **Two code comments cite a document that no longer exists.** `file_size.rs:67` ("spec I15") and
    `:161` ("spec I14") justify the two most surprising behaviors in file sizing by reference to the
    findings doc deleted in `76ca304`. Combined with SF-9, a reader has no authoritative record to
    check. Fold both rationales into the design doc as recorded amendments.
18. **The completion path re-verifies an entire package graph on every popup.** `build_memo.rs:96-101`
    clones the value *and* the full fingerprint vector under the mutex on every hit, then runs
    `check_fingerprints_strict`, which **re-reads and re-hashes every non-`node_modules` file** and
    stats every `node_modules` one. For a first-party package that means re-hashing its whole source
    graph on each keystroke inside an import's braces. Consider `Arc<V>` for the memo value and a
    short re-verify throttle, as the file-size cache already does with its TTL.
19. **`first_party_module_token` stats every first-party loaded path, per import, per poll.**
    `file_size_cache.rs:214-226` runs one `fs::metadata` per loaded first-party module for every
    import, on every file-size signature computation — which happens *before* the L1 hit check. A
    document with 10 imports into a 300-module first-party package is 3,000 stats per poll.
20. **Manifest work is done three times per named-import build.** `first_party_manifests` walks all
    loaded paths once at `analyze.rs:412` and again via `full_package_fingerprints`
    (`analyze.rs:118`), and the resulting manifests are then read and hashed a *third* time in
    `service.rs:2497-2502`. Separately, `top_module_contributions` (`analyze.rs:623`) clones the
    entire contribution vector to keep ten entries.
21. **Fixed option data is rebuilt on every single build.** `builtin_external()` (`adapter.rs:138`)
    allocates ~180 `String`s per build; `mapped_resolve_options` (`adapter.rs:156`) rebuilds the
    condition/extension/main-field vectors per build; `sorted_loaded_paths()` clones the full path
    set twice per successful build. All are `LazyLock` candidates.
22. **The miss drain spawns fresh OS threads on every call.** `scheduling.rs:34-50` spawns scoped
    threads per `drain_*`, and the calling thread then just waits — so a *single* cache miss (the
    common interactive case) spawns a thread to do work the caller could have done inline. Worse, a
    workspace report calls into this once per file from inside the report Rayon pool, so a 500-file
    report performs up to 500 × 4 thread creations while every report worker sits blocked. Run
    inline when there is one item, and back the drain with a persistent pool.

## 6b. Design-level gaps (beyond this release; recorded so they are not lost)

These are not defects against the spec — they are places where **the spec answered a question the
user did not ask**. They are the strongest ideas surfaced during the review and they belong on the
record.

1. **The product reports absolute cost; developers ask about marginal cost.** An import's number is
   computed as though the app were empty. If another file already imports `zod`, adding it here
   costs *approximately nothing* — the bundle already contains it. Shared-dependency accounting
   exists only *within a single file* (`shared_bytes`); there is no project-level model of "what is
   already in the bundle". The workspace report already walks every import in the project and so
   already holds the raw material for that union set. This is the highest-value idea absent from the
   design, and it is what EXT-1 is really a symptom of: the product has no concept of a *bundle*, only
   of an *import*.
2. **Nobody decided which build is being measured — and the answer came out "development".** The
   platform is `Neutral` and no defines are injected (both correct for *neutrality*), which means no
   `NODE_ENV` replacement, which means every library's dev-only branches are counted. React is the
   loud case. Users care about what ships. Honoring the project's production conditions/defines is
   *configuration of a public bundler option*, not a reimplementation of semantics, so it is squarely
   allowed by §14.6 — the design simply never asked. Related: the user's real bundler config (ES
   target, browserslist) is never read, and some of the 2.6–13% spread against the esbuild oracle
   probably lives here.
3. **The same module graph is re-linked over and over.** Ten different named imports from `date-fns`
   each load its ~300 files from scratch, plus the full-package comparison build. The memos cache
   *results* (a byte count, an export list), never the parsed graph. Whether Rolldown 1.1.5 exposes
   any graph or module reuse is unknown and worth actually establishing — this is a larger perf lever
   than raising `ENGINE_PERMITS`, which is the one everyone reaches for first.
4. **The failure cliff is too steep.** When a build blows a limit, the fallback measures *the entry
   file alone* — for a large UI kit that is not conservative, it is wrong by orders of magnitude while
   still looking like a number. There is no middle tier that reports an honest lower bound ("at least
   4 MB; graph limit exceeded").
5. **Non-JavaScript cost is invisible.** CSS, wasm, and font assets shipped by a package are real
   bytes in a real bundle and are never counted. This is not listed as a non-goal; it simply never
   came up.
6. **Duplicate package versions are not modeled.** Two versions of one package in the tree means the
   real bundle ships both — exactly the kind of finding that would make this product indispensable,
   and it is never surfaced.

## 7. Recommended sequence

Blockers first, in dependency order, then the reporting-fidelity cluster:

0. **EXT-2, then EXT-1** — the aggregates. EXT-2 is nearly free (use the total the controller
   already fetches) and stops the editor raising false budget warnings. EXT-1 needs a decision:
   deduplicate the workspace total by module, or relabel it honestly. Both are ahead of the engine
   work because they are the only findings where the product is *currently showing a user a number
   that is not a real quantity*.
1. **RB-1** panic containment + `IN_FLIGHT` drop-guard (stability; smallest diff, biggest risk
   retired).
2. **RB-2** wire `candidate_performance` into `validate.yml` — *before* the perf work in §6, so
   the improvements are measured by a gate that actually runs.
3. **SF-2 → SF-1** write the failing production-shaped side-effects row first, then supply
   `package_json_path` to make it pass. In that order: the test is the proof.
4. **SF-3** drop `|| is_array()` and pin the array reporting semantics with a test.
5. **SF-10 → SF-11** the user-visible wrong-answer cluster: stop calling cross-runtime modules
   "shared", and carry the runtime into export enumeration. Both mislead the user today.
6. **SF-4, SF-5, SF-7** correctness/robustness cluster.
7. **SF-12** build the `truly_treeshakeable` real-package baseline that Phase 4 claims exists.

   *(**SF-8 and the documentation half of SF-9 are DONE**, 2026-07-12: the SRS now states
   `rolldown2`, records the retained reporting-only side-effect matcher, and retracts the refuted
   Windows-glob watch item; the design doc's status header is corrected, §6.3 carries the I15
   per-runtime amendment and the I14 loaded-paths amendment, and §8.2 carries a new **aggregation
   rule** forbidding the summation of per-import compressed figures — the hole EXT-1/EXT-2 fell
   through. What remains of SF-9 is the code-side consequence: SF-10.)*
8. **RB-3** packaging, daemon-hash refresh, VSIX size check — last, because the daemon binary
   changes with every fix above.
9. §6 improvements, starting with `ENGINE_PERMITS`.
