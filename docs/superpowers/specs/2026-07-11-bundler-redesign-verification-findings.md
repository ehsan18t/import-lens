# Bundler Redesign — Post-Cutover Verification Findings

Status: **original review complete, 2026-07-11; second validation complete,
2026-07-12.** Independent verification of the shipped
`bundler-redesign` branch against the approved design
(`2026-07-10-bundler-redesign-design.md`) **only** — plan documents were
deliberately excluded so the review is not anchored to the implementation's own
narrative. Target goals under assessment: **most stable** and **best performant**.

## Method

- **Yardstick:** the approved design spec (§4–§15) plus its §10.7 qualification
  record. The five §10.7 "known divergences" are treated as already-accepted and
  are not re-reported as new defects.
- **Six independent read-only auditors**, each barred from the plan docs, covered:
  engine-contract conformance (§4–§8), lifecycle/failure/cutover (§9, §11, §12,
  §15), adversarial stability, adversarial performance, test coverage vs the
  §10.2 matrix + repo testing policy, and an honest old-vs-new behavior diff
  against the merge base (`f4460fa`).
- **Lead verification:** every load-bearing claim below was reproduced against the
  code (and against the merge base where a regression-vs-old-engine is claimed)
  before inclusion. Each finding is tagged **[confirmed]** (lead reproduced it) or
  **[reported]** (auditor-anchored, not independently re-run). Findings that did
  not survive reproduction were dropped. Where a 2026-07-12 validation note
  conflicts with an original tag or severity statement, the validation note is the
  current disposition.
- **Full suite:** `pnpm test` is **green** (exit 0). The 48-row construct matrix
  runs against the production engine in the default run (45 pass; rows 34/42/43
  `#[ignore]`d — see R8 and the accepted Windows divergences). In the default run,
  all 7 real-package tests and both candidate performance tests are fixture-gated
  and do **not** run (see R3).
- **Second validation (2026-07-12):** re-checked the current tree and merge-base
  claims, ran `pnpm test` and `cargo fmt --check`, explicitly ran matrix row 34,
  provisioned a clean pinned-fixture workspace and ran all 7 real-package tests,
  and ran the enforced accuracy oracle. Results: default suite green; row 34 green;
  real packages 7/7 green in a clean workspace; accuracy green at 2.6–13.0% delta.
  Packaging, daemon-hash regeneration, and the VSIX-size gate were not run.
- **Second-validation labels:** **[validation note]** corrects classification,
  scope, or severity without discarding the underlying observation. **[NEW]** marks
  an issue first found during the second validation. Original identifiers are kept
  even when a finding moves sections, so the original reviewer can trace feedback.

Second-validation command record (the fixture directory was a new unique temp path):

```text
pnpm test
cargo fmt --check
cargo test -p import-lens-daemon --locked --test candidate_matrix matrix_34_total_source_limit -- --ignored --nocapture
$workspace = node scripts/prepare-candidate-fixtures.mjs <unique-temp-dir> | Select-Object -Last 1
$env:IMPORT_LENS_FIXTURES_WORKSPACE = $workspace.Trim()
cargo test -p import-lens-daemon --locked --test candidate_packages -- --ignored --nocapture
$env:IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES = "1"
pnpm test:accuracy
```

## Feedback requested from the original reviewer

- Does any release-build reproduction support keeping R1 as a redesign regression,
  rather than the hardening/profile decision it is reclassified as here?
- Are there end-to-end measurements quantifying R2's extra fingerprint pass or R7's
  all-hit/report wall-time effect? If so, attach the commands and raw results.
- Should I8/I9/I10/I11/I15 be fixed before release, or should the approved design be
  amended with explicit divergences? Their original post-release classification no
  longer matches the cited requirements.
- Can N1 be independently reproduced from an interrupted or incomplete default
  fixture directory? N2 no longer needs confirmation: its emitted-code mechanism and
  raw-only size movement were reproduced directly.
- If a prevalence percentage is proposed for W4, provide the counting script and
  define whether it counts package shapes, source imports, or actual affected users.

## Bottom line

The cutover is **materially complete, but not fully faithful to the spec.** Rolldown is the
sole semantic engine behind one daemon-wide two-permit async boundary; the custom
engine and its tests are gone; the failure table, limits, contract types, and
Rolldown isolation are substantially implemented; README and SRS describe the shipped
architecture. Direct conformance gaps remain in `sideEffects` metadata/reporting,
diagnostic formatting/staging, and mixed-runtime file sizing (I8/I9/I10/I11/I15).
No auditor or second validation found a crash-on-normal-input or data-corruption defect.
The correctness story is a real net win over the old engine (dangling under-counts
eliminated, zero-byte external re-exports fixed, oracle agreement).

**It is not yet release-ready**, for three classes of reason:

1. **Owner-deferred release mechanics** (packaging, daemon-hash refresh, VSIX size
   check) — already acknowledged, and mandatory because the daemon binary changed.
2. **A short list of pre-release code items** — most importantly a per-miss
   whole-graph re-read that creates a confirmed freshness-correctness race and an
   unmeasured latency cost (R2), the deterministic type-position import failure
   formerly listed as W4, systematic raw-size inflation from Rolldown debug comments
   (N2), duplicate full-package builds (R4), missing manifest freshness inputs (R5),
   mixed-runtime aggregate sizing (I15), and the direct conformance gaps in I8–I11.
3. **Dark regression guards**: the real-package correctness gates that justify the
   whole redesign never run in CI (R3).

The `panic = "abort"` interaction originally ranked as R1 does not qualify as a
confirmed redesign regression. The old engine already processed arbitrary transitive
source through OXC, and switching to unwind would not recover stack overflow, OOM, or
explicit aborts. It remains a worthwhile hardening/profile decision and is moved to
Section D with the reasons preserved.

The performance picture deserves a caveat: the §10.7 qualification numbers measured
`RolldownEngine::bundle()` in isolation on a default-width runtime. The **shipped**
miss path wraps that in a whole-graph fingerprint re-read (R2), an optional second
full-package build (R4), a per-path canonicalize storm (I1), and post-processing
that runs off the build permit — none measured — and runs Rolldown on a 2-thread
runtime (I4). So "candidate is 0.54× the old engine" and "605 ms / 78 MB batch"
describe a narrower path than what ships. The engine is measured faster per isolated
build; whether, and by how much, the end-to-end miss path gives that advantage back
has not been characterized.

Raw-size qualification has a separate confirmed defect (N2): Rolldown's default
`attach_debug_info=Simple` adds module-region comments to the unminified chunk, while
`raw_bytes` is the chunk length and rendered-length module contributions include most
of the same wrapper bytes. Minified and compressed measurements remove those comments
and are unaffected in the reproduced case, which is why the accuracy oracle did not
expose either the raw inflation or the module-breakdown contamination.

Severity legend: **R** = fix/decide before release · **I** = improvement
opportunity under the original review (a validation note may elevate it) ·
**B** = better-than-before · **W** = changed/worse vs old (mostly spec-sanctioned;
W4 is promoted) · **N** = issue first discovered during second validation.

---

## A. Missing from spec

The contract is implemented with unusual fidelity, but the second validation found
additional partial-conformance items. They retain their original locations and IDs
for reviewer traceability:

- **§8.3 (freshness inputs) — partial.** Only the *root* package manifest is
  fingerprinted; transitive/first-party manifests consulted during resolution are
  not. → **R5**.
- **§7.3 (reject oversized files before returning source) — mechanism gap.**
  Enforcement happens after load+parse, not in the resolve/load hook. → **R6**.
- **§8.4 vs §5 tension.** The `enumerate_exports` signature returns `Vec<String>`
  with no channel for the ambiguous/external-only *diagnostics* §8.4 wants surfaced
  on success. The implementation follows §5 to the letter; the spec is internally
  tense here. → **I7**.
- **§8.1/§8.2 (raw measurement and module attribution) — contaminated by enabled
  debug metadata.** Rolldown wraps non-empty rendered modules and runtime output in
  `//#region` comments by default; those bytes are counted in `raw_bytes`, and most
  are also charged inside rendered-length module contributions. → **N2 [NEW]**.
- **§5.1 (stable diagnostics) — partial.** OXC diagnostics are formatted through
  unstable `Debug` output. → **I10** **[validation note: underestimated]**.
- **§6.3 (combined sizing) — partial for mixed-runtime component files.** Astro can
  produce Server and Client imports in one file, but the combined request applies
  the first resolved import's runtime to every entry. → **I15** **[validation note:
  underestimated]**.
- **§7.4/§14.6/§15 (`sideEffects` ownership) — partial.** A custom glob matcher
  remains in the resolver for reporting/static fallback, despite the spec forbidding
  Import Lens from matching these globs. String-form metadata also falls into
  `Unknown`. → **I8/I9** **[validation note: underestimated]**.
- **§12 (failure-stage preservation) — partial.** An OXC `minify` failure is surfaced
  under `engine_fallback`, contrary to the failure table's OXC-stage requirement.
  → **I11** **[validation note: underestimated]**.

The Appendix remains a useful conformance inventory, subject to these explicit
exceptions and the release-gate corrections in Section F.

---

## B. Better than before  *(your "features better than before")*

All confirmed against the merge base (`f4460fa`) unless noted.

1. **Dangling-binding under-counts eliminated — the central win. [confirmed]**
   The old engine emitted reads of generated `__il_` bindings no module declared
   (`date-fns/format` under-reported 33.2%→8.7%; `css-tree/parse` still emitted 4
   undeclared bindings at the merge base), because three hand-enumerations
   (`reachability.rs`, inclusion, emission) could disagree and emission "invented
   the name it expected." Rolldown owns linking now; the §10.7 record shows **zero**
   dangling bindings on all real packages including `css-tree/parse`.

2. **Zero-byte external re-exports fixed. [confirmed]** An explicit re-export from
   an external module used to emit an empty (0-byte) bundle (spec §2.2). Now every
   external is preserved as an import boundary with a diagnostic
   ([adapter.rs:174-179](../../../daemon/src/engine/adapter.rs#L174)); the user sees a
   real, boundary-preserving size.

3. **Size agrees with an independent oracle. [reported]** The old over-count
   fallback (`include_all_static_imports` kept *all* imports when inclusion could
   not decide) is gone; measured deltas are 2.6–13% vs an esbuild oracle
   (§10.7) versus the old 8.7–33.2% deviations.

4. **~2× faster per build. [reported]** Cold `css-tree/parse` p95 52.4 ms vs the old
   engine's 97.7 ms on the same fixture (§10.7). (But see the end-to-end caveat in
   the bottom line and R2/R4/I1/I4.)

5. **Real CJS interop instead of regex-scan + IIFE concatenation. [confirmed]** The
   old path wrapped each `require`d module in `;(() => { … })();` and concatenated
   (`cjs.rs`); named CJS access now resolves through Rolldown's link-time interop.

6. **Node-builtin list is now a single source of truth. [confirmed]** Extracted to a
   sorted `NODE_BUILTIN_MODULES` const with a sortedness guard test
   ([node_builtins.rs](../../../daemon/src/pipeline/node_builtins.rs)); it now also
   drives Rolldown's external list in both bare and `node:` forms
   ([adapter.rs:110-117](../../../daemon/src/engine/adapter.rs#L110)), so builtins are
   consistently externalized during the actual build, not only in diagnostics.

7. **Limits enforced incrementally, mid-build. [confirmed]** The three graph limits
   fire inside the plugin's `module_parsed` hook with a typed `module_graph_limit`
   breach ([plugin.rs:171-209](../../../daemon/src/engine/plugin.rs#L171)) rather than
   after a full custom graph walk. (Caveat: the per-module size check lands after
   parse — R6.)

8. **Unified freshness across ESM and CJS. [confirmed]** One `(path, runtime)`
   loaded-path index replaces the old split `GRAPH_CACHE` + bespoke
   `CJS_MODULE_CACHE`; constant-inlined modules still stay in `loaded_paths` so
   first-party edits invalidate.

**Injection-safety is a quiet win worth stating:** the virtual entry serializes
every user-controlled name/specifier through `serde_json::to_string` and uses
strictly positional aliases, so no package name can break out of the export clause
or collide with a generated alias ([entry.rs:16-45](../../../daemon/src/engine/entry.rs#L16),
tested at [entry.rs:128](../../../daemon/src/engine/entry.rs#L128)). **[confirmed]**

---

## C. Fix or decide before release  *(your "anything to fix/refactor before release")*

Ordered by combined impact on the two goals. Each is a hypothesis you should
re-confirm against the code, not a blind directive — evidence and reproduction
status are given.

### W4 — Valid TypeScript type-position imports become hard zero-size errors  *(correctness + user-visible failure)*  [moved from Section E; confirmed]

A TypeScript import can omit the `type` keyword while using the binding only in a
type position when the project's compiler/bundler performs legacy type-import
elision. For example:

```ts
import { ParseOptions } from "commander";
const options: ParseOptions = {};
```

`ParseOptions` exists in Commander's declarations but not its runtime ESM exports.
The document detector filters only imports OXC marks explicitly `is_type`; it sends
the example above as a runtime `Named(["ParseOptions"])` request
([imports.rs:96-100](../../../daemon/src/document/imports.rs#L96)). Rolldown correctly
reports that the runtime export is missing, but
[analyze.rs:173](../../../daemon/src/pipeline/analyze.rs#L173) converts that into a
hard error with all size fields zero. The file-size path then reports that no import
could be sized conservatively.

The second validation reproduced this exact case against `commander@12.1.0` through
the live daemon: `missing_export`, zero raw/minified/compressed fields, and aggregate
file-size failure. The old engine instead fell back to a usable full-package
measurement plus an export caveat when named reachability produced an empty bundle.

**Why this moved from Section E:** this is not merely a hypothetical compatibility
choice. It is deterministic on valid TypeScript accepted under import-elision
semantics and turns a source-level classification limitation into a hard product
failure. It belongs in the highest pre-release tier. However, the old full-package
fallback was only conservative, not necessarily the semantically correct size: when
TypeScript erases the import, the true runtime cost is zero. The correct fix is to
recognize type-position-only bindings (respecting the document's TypeScript mode) or
return a non-fatal type-only/unknown result—not blindly restore the old full-package
number. No prevalence percentage is accepted without a reproducible counting script
and a definition of what was counted.

### R2 — Every cold miss re-reads the whole module graph to fingerprint it, reintroducing a read-after-measure staleness race  *(performance + correctness)*  [confirmed vs merge base]

`dependency_fingerprints` now runs `file_fingerprint_reading_hash` — a full
`fs::read` + content hash — over **every** loaded path
([service.rs:2394-2397](../../../daemon/src/service.rs#L2394),
[cache/key.rs:408-414](../../../daemon/src/cache/key.rs#L408)). The old function
threaded content hashes out of "the EXACT graph instance the result was computed
from … with no second fetch that could rebuild against a dependency that changed
during the analysis window" (merge-base `service.rs` `fingerprints_with_content_hashes(paths, graph)`);
`file_fingerprint_reading_hash`'s own doc still says it is "for fallback paths …
that carry no read-time hash." Rolldown owns the graph and does not expose read-time
hashes, so the shipped path re-reads the whole loaded graph right after Rolldown
already read it. The product limits permit as many as 2,000 modules and accumulate
up to 100 MiB of Rolldown module code, but those are design ceilings rather than an
observed per-miss range or an exact ceiling on raw disk bytes. The clean real-package
run observed 2–640 loaded paths; bytes reread were not measured.

This is **two** regressions in one:
- **Latency:** an O(graph-bytes) disk pass on every cold import, unmeasured by the
  §10.7 harness (which timed only `bundle()`). Material on Windows / cold FS.
- **Freshness correctness:** re-reading *after* measurement reopens exactly the race
  the old comment was written to prevent — if a dependency changes between the build
  and the fingerprint read, the stored fingerprint describes newer bytes than were
  measured, so a later probe sees "fresh" and serves a **stale size**. The
  node_modules generation gate backstops installs, but first-party files are exposed.

**[validation note: impact split]** The second validation confirms the extra
O(graph-bytes) read/hash pass and the read-after-measure stale-serve race by control
flow and merge-base comparison. It did **not** benchmark the shipped end-to-end miss
path, so "material on Windows" is a plausible impact hypothesis, not a reproduced
latency result. R2 still qualifies for pre-release treatment on correctness alone.
The stale result is **sticky**, not a one-probe window: the stored fingerprint contains
the newer bytes' hash/metadata while the cached size describes the older build. Later
strict first-party probes re-read the newer file, match that newer hash, and continue
returning `Fresh` until another edit or explicit invalidation occurs. The unused
`file_fingerprint_from_read_time` helper and its post-analysis-TOCTOU documentation
are remnants of the guarantee the cutover stopped using.

Compounding this: the two deleted freshness guards
(`fingerprints_capture_read_time_len_not_post_analysis_stat` and the raw-`.ts`-byte
hashing tripwire, see R8) protected precisely this property and were removed with the
old engine — so the regression shipped with its guard test gone.

**Fix direction:** hash in the plugin's `module_parsed` hook where `module_info.code`
is already in memory (guarding that the hash equals raw disk bytes for plain JS; use
stat-only for transformed types), or capture stat-only fingerprints for node_modules
(already the cheap pre-filter) and reserve read-hashing for first-party; at minimum
rayon-parallelize the pass and re-home a read-time-capture guard test.

### N2 [NEW] — Rolldown debug-region comments systematically inflate raw size and module contributions  *(correctness + measurement)*  [confirmed]

The adapter leaves Rolldown's experimental debug option unset in
[adapter.rs:88-105](../../../daemon/src/engine/adapter.rs#L88). Rolldown 1.1.5 then
normalizes the missing value to `AttachDebugInfo::Simple`. Its module renderer wraps
every non-empty rendered module, plus emitted runtime output, in:

```js
//#region <debug_id>
// module output
//#endregion
```

`raw_bytes` is the final unminified chunk length, so these bundler-owned debug comments
are counted as package cost on every successful build. They are not source bytes and
do not belong in the user-facing raw measurement. The scope is non-empty **rendered**
modules, not every loaded path; modules removed completely by tree-shaking produce no
rendered wrapper.

The second validation inspected two actual `css-tree/parse` artifacts:

- 124 region-start and 124 region-end comments;
- 6,389 debug-comment bytes inside a 326,844-byte raw chunk (~1.95%);
- 123 module contributions summing to 326,214 bytes, only 630 bytes below the raw
  chunk;
- moving the fixture to a root 42 characters longer increased raw size by exactly
  42 bytes (`326,844 → 326,886`);
- both artifacts minified to exactly 197,787 bytes.

The contribution total proves this is also a module-breakdown defect, not only a raw
total defect: the 6,389 debug bytes exceed the entire raw-minus-contributions gap by
5,759 bytes, so at least that much bundler metadata is charged inside rendered-length
module contributions. Ordinary `sum(contributions) != raw_bytes` is explicitly
allowed by §8.2/§14.5 because chunk glue and runtime bytes are not attributable to a
real module; that expected approximation does not justify attributing debug wrappers
as package-module cost. Removing debug attachment will move contribution values as
well as `raw_bytes`.

The cross-root delta occurs once because the pre-resolved entry id is the verbatim
`\\?\C:\...` path produced by canonicalization, while the bundler `cwd` is not in the
same verbatim form. Rolldown's `stabilize_id(module_id, cwd)` therefore cannot
relativize that entry and emits one absolute entry `//#region`; modules Rolldown
resolves itself receive stable relative debug ids. This explains both the exact
42-byte delta and why it is not multiplied by module count.

**Why this moved to Section C:** N2 is not merely cross-root fixture noise. It is a
systematic error in a user-facing size field on every successful raw measurement.
Brotli is the default primary inline metric, so the UI impact is narrower than saying
raw is the default primary display; nevertheless raw appears in tooltips/history and
the protocol/SRS presents it as unpacked package cost. Existing accuracy gates compare
compressed minified output, where OXC strips the comments, and therefore cannot detect
this defect.

**Fix and gates:** explicitly set `attach_debug_info` to
`Some(AttachDebugInfo::None)` in `build_options`; assert production artifacts contain
no `//#region`/`//#endregion`; add a cross-root determinism test over every size field;
re-baseline §10.7 raw and module-contribution figures; and bump `ANALYZER_REVISION`
(or otherwise invalidate pre-fix cached measurements) because measured output
changes. Confirm that minified/gzip/Brotli/zstd remain unchanged.

### R3 — The real-package correctness gates never run in CI  *(stability / regression guard)*  [confirmed]

Every row in [candidate_packages.rs](../../../daemon/tests/candidate_packages.rs) is
`#[ignore]` + fixture-gated (`IMPORT_LENS_FIXTURES_WORKSPACE`), and nothing in the
automated path installs the fixtures or passes `--ignored`: `test:rust` is
`cargo test --workspace --locked` (no `--ignored`), pre-push is `pnpm test`, and CI
(`ci.yml → validate.yml → pnpm test`) runs the same. The full-suite run confirms
7/7 ignored. So the spec's named gates — "the four css-tree danglers reach zero" and
"`loaded_paths` includes tree-shaken dependencies" (date-fns) — exist **only** in a
suite that never executes automatically. The live synthetic matrix asserts
no-danglers on hand-written constructs that by design do not reproduce the real
`css-tree` §2.2 defect. A future compiler-stack bump reintroducing danglers on real
packages would ship green.

Partial mitigation exists: CI runs `pnpm test:accuracy` (`run_accuracy: true` in
`validate.yml`) over `scripts/accuracy-fixtures` (css-tree/date-fns/lodash) comparing
byte deltas to the esbuild oracle — an indirect real-package tripwire. But it does
not assert the specific dangler/tree-shaken-freshness contract. Its default relative
tolerance is **75%** (`IMPORT_LENS_ACCURACY_TOLERANCE ?? "0.75"`) while the observed
accepted deltas are 2.6–13.0%, so it is only a catastrophic-drift backstop; it is not
a credible substitute for the missing exact real-package assertions.

**Fix direction:** add a CI job that runs `node scripts/prepare-candidate-fixtures.mjs`
(from a committed lockfile, but with registry/store access for installation) and then
`candidate_packages -- --ignored`, or re-home
the zero-danglers + date-fns-freshness assertions into a live suite.

**[validation note: suite health]** The second validation explicitly ran the suite.
It passes 7/7 from a clean fixture directory, so this is an automation/guard-visibility
defect, not evidence that the real-package contract currently fails. Reusing the
preparer's default fixed temp directory did expose a separate reproducibility problem
recorded as **N1 [NEW]** in Section D.

### R4 — A second full-package build runs per named-import cache key  *(performance)*  [confirmed]

For every non-side-effectful `Named` import, `truly_treeshakeable` triggers a second
`bundle_sync(BundleSelection::Full)` + a second full minify, and only its `len()` is
used ([analyze.rs:248-278](../../../daemon/src/pipeline/analyze.rs#L248)). The cache key
includes the named set, so `import { a }` and `import { b }` from one package each
redo the identical whole-package build; single-flight is keyed per cache key and
cannot coalesce them. For **N** named variants of one entry/runtime, the shipped
per-import path performs **2N complete Rolldown builds** (N selected builds + N full
comparison builds), with a further combined build when file-size aggregation misses.
The old engine built/cached the graph once, performed the N selected in-memory
emissions, and memoized one full-package minified comparison in a `OnceLock` on that
cached graph. "Roughly doubles" therefore understates the expensive part: the new
full comparison reloads and relinks the graph N times instead of computing once from
cached graph state. **Fix direction:**
memoize the full-package minified size per `(entry_path, runtime)` behind the same
freshness fingerprints, or single-flight it on package identity.

### R5 — First-party manifest edits are not caught by freshness (§8.3)  *(correctness)*  [confirmed]

Only `package_root.join("package.json")` (the root manifest) is appended to the
fingerprint set ([analyze.rs:290-293](../../../daemon/src/pipeline/analyze.rs#L290)); the
plugin records only real graph *modules*, so a `package.json` consulted during
resolution/side-effect classification for a **first-party workspace** dependency is
never fingerprinted. Editing that manifest's `exports`/`type`/`sideEffects` changes
resolution and retention while no fingerprinted path moves → a stale size is served
as fresh. (node_modules manifests are covered by the install-generation gate; the
exposure is first-party.) **Fix direction:** derive the nearest `package.json` for
each distinct first-party directory in `loaded_paths` and add them to the fingerprint
set — pure product-side path bookkeeping, no bundler semantics.

### R6 — Oversized modules are rejected only after full read + parse on several build paths (§7.3)  *(stability / resource)*  [confirmed]

The `load` hook returns `None` for every real file (no size check,
[plugin.rs:145-153](../../../daemon/src/engine/plugin.rs#L145)); the 20 MiB per-module
limit is checked from `module_info.code`, which exists only after Rolldown reads and
OXC parses the file ([plugin.rs:171-179](../../../daemon/src/engine/plugin.rs#L171)). Spec
§7.3 wants the resolve/load hook to reject "before returning its source when
possible." A pathological multi-hundred-MB generated file is fully read into memory
and parsed before the typed failure fires — the memory bound the limit exists to
enforce is exceeded before it engages. The per-import analysis path has an early
root-entry metadata guard ([analyze.rs:127-149](../../../daemon/src/pipeline/analyze.rs#L127)),
but the combined file-size path has none
([file_size.rs:56-147](../../../daemon/src/pipeline/file_size.rs#L56)), and export
enumeration/completion passes its resolved root directly into the engine
([service.rs:1545-1613](../../../daemon/src/service.rs#L1545)). Therefore transitive
modules are exposed on every path, while oversized root entries are also exposed on
combined file sizing and export enumeration/prewarm. The failure itself stays
structured (no partial graph), so this is a mechanism gap, not a correctness bug.
**Fix direction:** `std::fs::metadata(id)` in the `load`/`resolve_id` hook for
path-like ids and raise the breach when `len() > MAX_MODULE_SOURCE_BYTES`, keeping
`module_parsed` as the total-bytes authority and the per-import guard as a cheap
earlier diversion to static sizing.

**[validation note: regression scope]** This qualifies as a §7.3/resource-bound gap,
but not as a wholly new full-read regression: the old graph engine also read a module
before enforcing its byte limit. The redesign makes the timing worse because the
check now occurs after Rolldown/OXC parsing. The early root metadata guard applies
only to per-import analysis; it does not narrow the combined-size or enumeration
surfaces described above.

### R7 — Analysis parallelism for cache hits and reports collapsed from the pool to 2 workers  *(performance)*  [confirmed vs merge base]

`handle_batch`, `handle_batch_streaming`, and `handle_file_size` wrap the whole
`analyze_with_cache` call — **including the cache-hit path** — in `drain_ordered`,
which caps workers at `ENGINE_PERMITS = 2`
([service.rs:610](../../../daemon/src/service.rs#L610),
[:642](../../../daemon/src/service.rs#L642), [:674](../../../daemon/src/service.rs#L674),
[scheduling.rs:21](../../../daemon/src/engine/scheduling.rs#L21)). The old code fanned
these over the full Rayon pool (`par_iter()`). Spec §9 only requires that *builds*
be limited to 2 and never launch from the outer Rayon loop — it does not require
cache hits to serialize. A 20-import all-hit batch now serves hits 2-wide. More
significantly, the workspace-report builder uses the same drain around file reading,
parsing, import detection, and analysis
([service.rs:491-497](../../../daemon/src/service.rs#L491)), so all of that document
work also runs at no more than two-wide concurrency. Per-hit latency can remain below
its gate while aggregate batch/report wall time still changes; no numeric regression
is asserted without a benchmark. The correct pattern already exists in the repo:
`analyze_package_json` classifies hits pool-wide (`par_iter` at
[service.rs:1296](../../../daemon/src/service.rs#L1296)) and drains only misses 2-wide.
**Fix direction:** port that two-phase split to the batch/file-size/report handlers,
or give `drain_ordered` more workers than permits (the semaphore still caps builds).

**[validation note: overestimated impact]** The reduction in available outer
parallelism is confirmed: these paths changed from the Rayon pool to exactly two
workers, including cache-hit and document/report work. The original `~cores/2`
wall-time estimate was not benchmarked and did not account for serial work,
filesystem contention, or the new engine's different per-item cost, so it is removed
from the finding rather than left as an inline assertion. Keep R7 here, but treat the
performance magnitude as uncharacterized until an all-hit batch and a representative
workspace report are measured old-vs-new.

### R8 — Dark limit branch and un-re-homed freshness guards  *(test coverage)*  [confirmed]

- **Total-source limit is dark.** Matrix row 34 (`MAX_GRAPH_SOURCE_BYTES`, the only
  coverage of the total-byte accumulator branch,
  [plugin.rs:186](../../../daemon/src/engine/plugin.rs#L186)) is `#[ignore]`d ("writes
  >100 MiB"), and no automated entry point runs the matrix with `--ignored`. Rows 32/33
  cover the count/per-module branches, so `module_graph_limit` is only partially live,
  contradicting §10.4 "all graph limits observable and deterministic." **Fix:** gate
  row 34's fixture size behind an env flag and run it in a nightly job, or shrink the
  limit under a test cfg so the branch runs by default.
- **Two freshness guards deleted without equivalent** (ties to R2). The merge base's
  `module_graph_carries_content_hash_for_loaded_modules` and
  `fingerprints_capture_read_time_len_not_post_analysis_stat` asserted §8.3 behavior
  (raw pre-transform `.ts` hashing; read-time len/mtime capture, not a post-analysis
  stat). They referenced now-deleted symbols so removal was forced, but the
  *assertions* are freshness, not old-engine semantics, and no located test
  re-establishes them on the new engine fingerprint path. **Fix:** re-home a
  `.ts`-dep raw-byte-hash assertion and a read-time-len assertion onto the current
  path.

**[validation note: branch works, guard remains dark]** The second validation ran
`matrix_34_total_source_limit` explicitly and it passed. R8 therefore concerns
continuous regression coverage, not a currently failing accumulator. The two deleted
freshness properties still have no equivalent test on the shipped path.

---

## D. Improvements / opportunities  *(your "anything that can be improved")*

Ordered roughly by value toward the two goals. These were originally classified as
post-release acceptable. A **[validation note: underestimated]** label means the
observation qualifies but its original disposition was too weak and should be
resolved before release or explicitly accepted as a spec divergence.

### R1 — `panic = "abort"` and unwind-based isolation  *(stability hardening; moved from Section C)*  [validation note: moved]

[Cargo.toml:13](../../../Cargo.toml#L13) sets `[profile.release] panic = "abort"`
(with `opt-level="z"`, `lto`, `strip` — a size profile). Under `abort`, the panic
branches of the daemon's isolation layers cannot recover a request: the
`spawn_blocking`→`JoinError` mapping
([ipc/server.rs:1025](../../../daemon/src/ipc/server.rs#L1025)), the `catch_unwind`
wrappers ([service.rs:445](../../../daemon/src/service.rs#L445),
[:536](../../../daemon/src/service.rs#L536)), single-flight leader-panic recovery
([analysis_flight.rs](../../../daemon/src/analysis_flight.rs)), and semaphore RAII
cleanup all require unwinding.

**Why this no longer qualifies as a confirmed pre-release redesign defect:**

- `panic = "abort"` predates the branch, and the old engine already passed arbitrary
  transitive dependency source through OXC parser, transformer, minifier, and
  codegen paths. Rolldown adds dependency and assertion surface, but arbitrary-source
  exposure itself is not new.
- No panic-inducing supported package or normal-input reproduction was found. The
  confirmed fact is the profile/isolation mismatch, not the predicted crash rate.
- `panic = "unwind"` would make ordinary `panic!`/`unwrap` failures recoverable, but
  it generally does **not** recover stack overflow, OOM, an explicit process abort,
  or a dependency compiled to abort. The original mitigation overstated its scope.
- The extension already restarts a crashed daemon with exponential backoff starting
  at 1 second and enters unavailable mode after 3 crashes in 60 seconds
  ([nativeTransport.ts:319-336](../../../extension/src/daemon/nativeTransport.ts#L319)).

**Disposition:** keep as an explicit release-profile/hardening decision, but do not
call it the highest-value stability regression without an actual panic reproduction
and a release-build size/recovery comparison. If unwind is selected, document its
limited failure coverage; if abort is retained, annotate unwind-only recovery code
and prioritize the transitive pre-parse size guard in R6. **Former I16 is folded into
this decision:** a poisoned `dependency_paths` lock cannot survive in the shipped
`panic = "abort"` process, so `lock_unpoisoned` has no current release effect. If the
profile changes to unwind, then align that lock with `analysis_flight` as part of the
same change; it is not a standalone defect under the current profile.

**Performance headroom (beyond meeting the gates):**

- **I1 — Per-path `fs::canonicalize` storm runs inside the build permit.**
  `sorted_loaded_paths` canonicalizes every loaded path
  ([plugin.rs:41-53](../../../daemon/src/engine/plugin.rs#L41)) from inside `translate`,
  which executes while holding the permit on an engine worker thread; on Windows each
  canonicalize is a file-handle open, extending permit hold time and potentially
  stalling the other in-flight build. No cross-build memo. **Fix:** canonicalize on
  the caller thread after permit release, and/or add a canonical-path memo invalidated
  with `dependency_paths`. [confirmed present] **[validation note: count
  overestimated]** The cost is one call per loaded path, not uniformly 300–2,000.
  The clean real-package run observed 2–640 loaded paths; 2,000 is the hard maximum,
  not a typical lower bound.
- **I2 — Post-build work prevents the same drain from refilling released permits.** With
  `workers == permits`, minify/compress/fingerprint/insert all run after `bundle_sync`
  returns but still occupy that drain's workers. Its next queued builds cannot submit
  while both workers post-process, so the drain may temporarily leave released
  permits unused. The permits are daemon-wide, however: unrelated interactive,
  prewarm, or SWR work can acquire them, so the original statement that "the 2
  permits sit idle" was too broad. **Fix:** drain with `permits + k` workers, or move
  post-processing to Rayon. [reported; validation-corrected]
- **I3 — No priority lane.** Background prewarm, SWR revalidation, and default-export
  probes share the same 2 FIFO permits as interactive misses
  ([boundary.rs:24](../../../daemon/src/engine/boundary.rs#L24)); during a prewarm sweep an
  interactive import waits behind background builds. **Fix:** split an interactive
  lane from a background lane (background capped at 1). [reported]
- **I4 — The engine runtime is 2 threads, capping Rolldown's *internal* parallelism —
  and qualification measured wider.** `worker_threads(ENGINE_PERMITS)`
  ([boundary.rs:33-43](../../../daemon/src/engine/boundary.rs#L33)) means both concurrent
  builds' module tasks share 2 OS threads on an 8–16-core machine, while
  `candidate_performance.rs` ran on a default multi-thread (num_cpus) runtime — so
  production is narrower than the recorded numbers. Permits bound memory; runtime
  width need not equal permits. **Fix (tunable, evidence-gated per §10.7):** keep
  `ENGINE_PERMITS = 2`, raise runtime workers to `min(num_cpus, 8)`, re-measure.
  [confirmed present] **[validation note]** The runtime-width mismatch is confirmed;
  the amount of production slowdown is not. Treat the proposed width as a benchmark
  candidate, not an established optimum.
- **I5 — Prewarm's default-export probe is a full engine build per dependency.**
  `exposes_default_export` runs `enumerate_exports_sync`
  ([prefetch.rs:203](../../../daemon/src/prefetch.rs#L203),
  [:232](../../../daemon/src/prefetch.rs#L232)); on first project open a 50-dep tree issues
  ~50 serialized builds before real prewarm starts, all competing for the 2 permits.
  Memoized only after the first sweep. **Fix:** a single-file OXC parse for
  `export default` (conservative `true` on re-export stars), or drop the probe and
  let the Default job fail once. [confirmed present]
- **I6 — File-size and enumeration redundancy.** The combined file-size build
  re-bundles every package the per-import analyses just built (a 1-import file does a
  second identical build — [file_size.rs:96](../../../daemon/src/pipeline/file_size.rs#L96)),
  and completion enumeration is an uncached full build per request
  ([service.rs:1545-1613](../../../daemon/src/service.rs#L1545)). The original rationale
  that an ImportSize artifact already contains reusable package export names is
  false: its virtual entry publicly exports positional aliases such as
  `__il_entry_0_export_0` ([entry.rs:16-45](../../../daemon/src/engine/entry.rs#L16)),
  whereas only the passthrough enumeration build makes the real package entry strict
  and yields its actual export names ([adapter.rs:48-71](../../../daemon/src/engine/adapter.rs#L48)).
  An actual export-list cache is therefore required; there is no free list to reuse.
  `BundleArtifact.exported_names` is populated for ImportSize artifacts but has no
  production reader under `daemon/src`—only contract/qualification tests inspect the
  generated aliases—so it should be deleted/narrowed or explicitly retained as a
  test invariant. Plus one avoidable full-chunk `String` clone remains per build
  ([adapter.rs:197](../../../daemon/src/engine/adapter.rs#L197)). **Fix:** fast-path
  single-import file sizing; add a bounded export-list cache keyed on entry stat token
  and runtime; decide whether to narrow the unused artifact field; `try_unwrap` the
  chunk `Arc` instead of cloning. [confirmed; rationale corrected]

**Contract/reporting precision (§7.4/§8.4):**

- **I7 — Enumeration drops Rolldown warnings on success**, so ambiguous-star /
  external-only omissions surface with no diagnostic ([adapter.rs:53-71](../../../daemon/src/engine/adapter.rs#L53),
  `diagnostics: Vec::new()` at [service.rs:1592](../../../daemon/src/service.rs#L1592)).
  Return `(Vec<String>, Vec<ImportDiagnostic>)` and translate the warnings. [confirmed]
- **I8 — String-form `sideEffects` lands in the `Unknown` bucket**, not treated as a
  one-pattern glob ([resolver.rs:681-687](../../../daemon/src/pipeline/resolver.rs#L681)),
  so the conservative glob-confidence diagnostic never fires for
  `"sideEffects": "./x.js"` even though it suffers the identical Windows undercount.
  [confirmed] **[validation note: underestimated and fix direction corrected]**
  This is not only diagnostic polish: §7.4 explicitly includes string-form metadata
  in the qualification surface. Do not simply map the string through the current
  array path if I9 is resolved by deleting the matcher. Instead, normalize string and
  array forms into conservative reporting/confidence metadata without locally
  deciding which files match. Resolve before release or record a separate accepted
  reporting divergence.
- **I9 — A custom `sideEffects` glob matcher still lives in the resolver**
  ([resolver.rs:44-67](../../../daemon/src/pipeline/resolver.rs#L44)), which §7.4 says
  should come from public metadata "rather than its own matcher" and §15 lists among
  cutover removals. It never reaches Rolldown (retention is untouched), so the hard
  rule holds — but it drives the static-fallback matched-path diagnostic. Delete it
  and reduce that diagnostic to the conservative message, or amend the spec to accept
  this reporting-only remnant. [confirmed] **[validation note: underestimated]**
  This directly contradicts §7.4, §14.6, Phase 3, and §15. Therefore the Section F
  claim that all §15 cutover-deletion bullets are done is false unless the matcher is
  removed or the design is amended. Its reporting-only role limits runtime severity;
  it does not make the conformance gap disappear.
- **I10 — OXC diagnostics are `Debug`-formatted into product messages**
  ([minify.rs:23](../../../daemon/src/pipeline/minify.rs#L23),
  [:37](../../../daemon/src/pipeline/minify.rs#L37)) — no Rolldown type leaks, but an
  unstable `Debug` of a movable dependency reaches users. Use `Display`/message
  accessors. [confirmed] **[validation note: underestimated]** §5.1 explicitly bans
  unstable debug representations, so it is not satisfied "to the letter." This is a
  direct contract gap and should be fixed before release or explicitly waived.
- **I11 — Stale confidence text** claims "full-graph sizing instead of named-export
  tree shaking" ([analyze.rs:544-548](../../../daemon/src/pipeline/analyze.rs#L544)),
  describing the *deleted* engine. More seriously,
  `engine_fallback_diagnostic` overwrites the stage of **every fallback-eligible
  engine failure** with `engine_fallback`, not only OXC minify failures. Resolve,
  parse, transform, link, generate, output-shape, graph-limit, OXC validation/minify,
  and other non-missing-export failures therefore lose the stable stage required by
  §12. Reword the confidence text and preserve the inner stage while separately
  indicating that static fallback was used. [confirmed; validation note:
  substantially underestimated]
- **I12 — Builtin list omits `node:test`/`node:test/reporters`/`node:sqlite`/`node:sea`/`trace_events`/`wasi`**
  ([node_builtins.rs](../../../daemon/src/pipeline/node_builtins.rs)), so those imports
  take the unresolved-import warning path (Medium confidence) instead of clean
  externalization. Add `trace_events`/`wasi` in bare and `node:` forms, and model
  `node:test`, `node:test/reporters`, `node:sqlite`, and `node:sea` as prefix-only
  externals. [reported] **[validation note: incomplete list corrected]** Node 24's
  `module.builtinModules` confirms the additional `node:test/reporters` omission and
  the distinction between ordinary and mandatory-prefix builtins.

**Stability defense-in-depth (all fail-safe; none corrupt data):**

- **I13 — Windows glob-`sideEffects` undercount is under-communicated by a
  platform-independent array diagnostic (§10.7 div. 1).** Every platform emits the
  generic "matched paths unavailable / confidence conservative" diagnostic whenever
  `reported_side_effects` has the array form
  ([adapter.rs:180-193](../../../daemon/src/engine/adapter.rs#L180)); only the actual
  glob-effectful-file undercount is Windows-specific. String-form metadata never
  reaches this diagnostic at all (I8). The message therefore neither identifies the
  Windows size risk nor covers the equivalent string form. **Spec-compliant fix
  (metadata only):** on Windows state "size may be undercounted: bundler cannot match
  glob `sideEffects`," extend conservative reporting to the string form, and drop
  confidence to **Low** for glob/string `sideEffects`; keep other platforms' wording
  platform-appropriate. [confirmed; scope corrected]
- **I14 — Combined file-size build records the union of all loaded paths under every
  entry key** ([file_size.rs:121-123](../../../daemon/src/pipeline/file_size.rs#L121)), so
  a later L1 file-size signature for a different document importing package A can
  include package B's paths and recompute when B changes. The union index has one
  consumer—`first_party_module_token` in the **L1 aggregate file-size cache**—and
  never feeds durable `dependency_fingerprints`. This is cross-document
  over-invalidation that fails safe: a cache-churn/performance wart, not a stale-size
  correctness risk. **Fix:** skip `record_loaded_paths` on the combined path, or
  record per-entry subsets. [confirmed; severity clarified]
- **I15 — Mixed-runtime combined file sizing locks the first request's runtime for
  all entries** ([file_size.rs:66-98](../../../daemon/src/pipeline/file_size.rs#L66)); a file
  importing a Server package and a Client package sizes both with the first's
  conditions, with no fallback if the mis-conditioned build still succeeds. **Fix:**
  group entries by runtime, one build each, or fall back when runtimes differ. [reported]
  **[validation note: underestimated and statically confirmed]** This is reachable,
  not hypothetical: the document pipeline intentionally emits Server imports from
  Astro frontmatter and Client imports from processed `<script>` blocks in the same
  file (`document_analysis.rs` already tests that shape). Per-import results use the
  correct runtime, but the aggregate file size can be wrong. Treat as a pre-release
  correctness item. See
  [document_analysis.rs](../../../daemon/tests/document_analysis.rs) for the existing
  mixed Astro-region test.
- **I17a — Contribution path display uses Rolldown's raw id while `loaded_paths` are
  canonicalized.** Internal membership remains consistent because the adapter
  canonicalizes only the freshness list; the observable difference is path spelling
  in module breakdowns. Information-level. [reported]
- **I17b — SWR revalidation is detached and can write after a shutdown flush.** The
  `spawn_blocking` task at
  [ipc/server.rs:900-923](../../../daemon/src/ipc/server.rs#L900) is not joined during
  shutdown, so it can complete and attempt a post-flush cache/outbound write. Existing
  cancellation and send failure keep this fail-safe; information-level. [reported]

**[validation note: removed I17 subclaim]** The former
`total_source_bytes`/double-`module_parsed` concern does not qualify. This adapter
creates one bundler, calls `generate()` once, and Rolldown invokes `module_parsed`
once from each module task; there is no current rebuild path that fires the hook twice
for one module. Unconditional accumulation is therefore not an observable defect.

**Testing and measurement reproducibility — new findings:**

- **N1 [NEW] — Fixture preparation reuses a fixed temp directory without repairing
  missing package files.** `prepare-candidate-fixtures.mjs` defaults to the stable
  `import-lens-candidate-fixtures` directory, copies the manifest/lockfile, and runs
  `pnpm install`, but does not recreate or integrity-check the target. The second
  validation's first explicit package run reused an incomplete prior installation:
  `lodash-es`, `react`, and `uuid` failed because expected local package files were
  absent while pnpm considered the install current. A new unique target installed
  the complete packages and passed 7/7. **Why this belongs here:** it does not
  indicate an engine correctness failure or a clean-CI failure, but it makes the
  documented local qualification command non-reproducible after an interrupted or
  corrupt setup.
  **Fix:** prepare into a unique directory, or remove/recreate the target's
  `node_modules` before the frozen install and verify the seven package entry files.

---

## E. Changed or worse vs the old engine  *(honest ledger)*

Every item here except W1 (inferred, no old baseline) and W5 (not runtime-verified)
is explicitly sanctioned by the §10.7 record or the SRS. Included for completeness so
nothing reads as sold rather than surfaced. W4 was removed from this ledger and moved
to the top of Section C after its valid TypeScript reproduction made the original
"needs-decision" framing untenable.

| # | Change (user-visible) | Severity | Sanctioned? |
| --- | --- | --- | --- |
| W1 | Wide cold batches may be slower wall-clock on many-core machines (pool→2). No old 20-import cold baseline was ever recorded, so this is inferred from code shape, not measured. | acceptable-tradeoff | §9/§14.3; ties to R7, I4 |
| W2 | Array-`sideEffects` packages always flagged side-effectful → lose "truly tree-shakeable" badge, drop to Medium confidence, even when the requested import touches no effectful file ([analyze.rs:234](../../../daemon/src/pipeline/analyze.rs#L234)). | acceptable-tradeoff | §7.4 + §10.7 div. 1 |
| W3 | The *which files* matched-side-effect detail is gone from diagnostics (`matched_side_effect_paths` always empty). | acceptable-tradeoff | §7.4 |
| W5 | Named-CJS typo detection weakened: the old `cjs_scan` "named CommonJS export not found" warning is gone (interop exposes a synthetic namespace). Not runtime-verified; offset by now-correct CJS sizes. | needs-decision | — |
| W6 | Module-breakdown attribution basis changed (rewritten-length → rendered-length); constant-inlined modules render 0 and drop off the breakdown. Occasionally fewer rows. | acceptable-tradeoff | §8.2 + §10.7 div. 3 |
| W7 | Multi-import file size sums per-import totals **without shared-module dedup** on the combined-build-failure fallback (over-counts shared code); the old engine had no such fallback (it zeroed the aggregate instead). | acceptable-tradeoff | SRS FR-024a |

CJS *completion* is **not** a regression: the old ESM-graph enumeration returned an
empty list for pure-CJS entries; the new path returns `default`-only — a marginal
improvement. Neither version ever completed named CJS members.

---

## F. Release gate checklist

| Gate | Status |
| --- | --- |
| Six-target packaging | **deferred (owner)** — mandatory; daemon binary changed |
| Daemon-hash refresh (`knownHashes.generated.ts`) | **deferred (owner)** — the committed hash predates the cutover commit and cannot match a Rolldown-era binary |
| VSIX size check vs SRS cap | **deferred (owner)** — headroom measured ample (§3.2) but unverified for the new binary |
| R1 panic profile decision | **moved to hardening/explicit decision** — profile mismatch confirmed; redesign regression and unwind coverage were overstated |
| W4 valid TypeScript type-position imports | **open, highest tier** — deterministic hard error/zero result reproduced; distinguish semantic type-only use from runtime typos |
| R2 fingerprint re-read / freshness race | **open** |
| N2 debug-comment measurement inflation | **open** — systematic raw-field and module-contribution errors reproduced; disable debug attachment, re-baseline both, and invalidate old measurements |
| R3 real-package gates run in CI | **open** |
| R4–R8 | **open** — row 34 passes explicitly, but its automated guard and the freshness guards remain dark |
| I8/I9/I10/I11/I15 validation underestimates | **open** — direct §5.1/§6.3/§7.4/§12/§15 conformance or correctness work; fix or explicitly amend/waive |
| §15 code/doc bullets (engine ownership, isolation, pins, README/SRS, cutover deletions, `ANALYZER_REVISION="rolldown1"`) | **partial** — core engine deletion/cutover is done; reporting-only custom `sideEffects` matcher remains (I9) |
| `pnpm test` + `cargo fmt --check` | **done** — green in second validation |
| Explicit matrix row 34 | **done for current tree** — passes; still not automated (R8) |
| Clean real-package qualification | **done for current tree** — 7/7 passes; still not in CI (R3), default fixture reuse is fragile (N1) |
| Enforced accuracy oracle | **done for current tree** — green at 2.6–13.0% delta, but its 75% tolerance and minified/compressed basis do not cover R3's exact assertions or N2's raw/contribution fields |

The three owner-deferred mechanics remain mandatory. They are not the only open
definition-of-done concerns: W4 is a deterministic valid-TypeScript failure; N2
systematically contaminates raw measurement and module attribution; I9 contradicts
the matcher-removal bullet; I10/I11 do not fully satisfy the diagnostic/failure
contract; I15 leaves a
reachable aggregate correctness gap; and R3/R8 remain non-executing regression
guards. The second validation proves the explicit row-34 and clean real-package tests
currently pass; it does not make them continuous gates.

---

## Appendix — verified conforming (so silence is not ambiguous)

Independently reproduced against the code and found correct, subject to the explicit
exceptions below:

- **Pins & isolation (§4):** `rolldown =1.1.5` (+ `rolldown_common`/`rolldown_error`),
  every direct OXC crate `=0.139.0`, `oxc_resolver =11.23.0`; no `rolldown-candidate`
  feature remnant; `rolldown::` imported only in `adapter.rs`/`plugin.rs`; no
  `oxc_ast`/`oxc_ast_visit`/`oxc_transformer` direct dep; no Rolldown type in any
  public/persistent surface.
- **Contract (§5/§5.1):** request/artifact/failure shapes match; one unminified ESM
  chunk; `loaded_paths` canonicalized/sorted/deduped incl. tree-shaken modules;
  contributions are rendered-length, rendered-only; `exported_names` comes from the
  chunk export list as required. For ImportSize that list contains the virtual
  entry's positional aliases, not reusable real package export names, and the field
  has no production consumer (I6); this does not make the current §5 shape
  nonconforming, but it does invalidate I6's original reuse rationale. Failures carry
  no code field; no custom star-export walker survives. Exception: OXC product
  diagnostics still use unstable `Debug` formatting (I10).
- **Virtual entry (§6):** synthetic `import-lens:target/N` → pre-resolved absolute
  path, no re-resolution; generated forms match spec (escaping-namespace form for
  Namespace/Full); serde_json escaping + positional aliases are injection-proof.
- **Options/plugin/limits (§7):** ESM, strict signatures, sourcemaps off, code
  splitting off (dynamic inlines), minify off; condition names/main fields mapped
  from the shared resolver; exactly-one-chunk `output_shape` fence; plugin does only
  its three jobs and returns no `HookSideEffects`; limits are 2000/20 MiB/100 MiB,
  per-build, thread-safe, monotonic, typed breach, no partial graph.
- **Measurement (§8):** `raw_bytes` = chunk length; parse-once → validate → minify →
  codegen, no `oxc_transformer`, no double build for minified output; compression over
  the one minified string. Exception: Rolldown defaults debug attachment to `Simple`,
  so raw chunks contain a region-comment pair for every non-empty rendered module and
  runtime output. N2 confirms 6,389 bytes of such overhead for `css-tree/parse` and
  one absolute entry debug id that makes raw size root-length dependent. Its 123
  module contributions sum to 326,214 of 326,844 raw bytes, proving that the debug
  wrappers also contaminate module attribution; the allowed contribution/raw gap is
  not itself a defect. Minification removes the comments in the reproduced case.
- **Lifecycle/failure (§9/§12):** one daemon-wide 2-permit semaphore; cache hits
  bypass it and construct no bundler; input order preserved; blocking work on
  `spawn_blocking` threads, not Tokio I/O; no symbol fabrication or partial-code
  measurement; static `manifest_fallback` estimator unchanged and still wired.
  Exception: every fallback-eligible engine failure loses its inner stage under
  `engine_fallback` (I11), and several outer callers limit even cache-hit work to two
  workers (R7).
- **Cutover (§11/§15):** `ANALYZER_REVISION = "rolldown1"`; `bundle.rs`/`cjs.rs`/
  `cjs_scan.rs`/`graph.rs`/`reachability.rs`/`replacements.rs` deleted; no leftover
  `module_exported_names`/`module_provides_export`/marker identifiers; no runtime
  dual-engine selection; `oxc-parser` removed from tsdown `neverBundle`; README/SRS
  describe Rolldown-on-OXC ownership truthfully. Exception: the reporting/static-
  fallback `sideEffects` matcher remains despite §15's deletion requirement (I9).
- **Concurrency soundness:** no `block_on` in async; one shared engine runtime (no
  per-request runtime); permit released on unwind (RAII); Tokio-FIFO semaphore keeps a
  batch from starving an interactive import; single-flight leader/follower correct;
  per-build plugin state isolated. (All modulo R1's `abort` caveat.)
- **Tests:** matrix wired to the **production** engine (no feature gate), 45 rows live;
  §10.3 package set complete and pinned with missing-fixture = loud panic (not silent
  skip); §10.5 compiler-stack coordination gates live on pre-push + CI; no Echo tests
  or illegal dependency-version assertions introduced. The second validation passed
  row 34 and the clean 7-package suite explicitly; R3/R8 describe why that coverage
  remains dark by default, and N1 records the fixed-temp setup weakness. No current
  gate covers implicit type-position-only imports (W4) or rejects production debug
  region comments (N2).
