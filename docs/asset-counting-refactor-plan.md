# Plan: simplify asset counting without changing behavior

**Status: refactor plan only. No implementation has started.**

This plan reduces the size and coupling of the asset-counting implementation while preserving the
behavior at commit `8fe2342`. It follows the contracts in
[ADR-0005](adr/0005-a-runtime-is-an-artifact-boundary.md),
[ADR-0006](adr/0006-the-result-model.md), and the findings in
[asset-counting-audit.md](asset-counting-audit.md).

The purpose is not to make the same code occupy more files. The refactor succeeds only if it removes
duplicated mechanisms, shrinks the interface callers must understand, deletes superseded tests, and
reduces total lines while keeping every current result, diagnostic, freshness rule, limit, and cache
decision unchanged.

## 1. Baseline and reduction target

The relevant feature baseline is `b29c329`, immediately before the asset-counting branch. The frozen
behavior point for this refactor is `8fe2342`.

| Range | Added | Deleted | Net |
| --- | ---: | ---: | ---: |
| Initial asset work through `7729faa` | 2,887 | 238 | +2,649 |
| Audit and follow-up fixes through `8fe2342` | 6,902 | 625 | +6,277 |
| Whole asset feature through `8fe2342` | 9,523 | 597 | +8,926 |

The two lean-orchestration commits after `8fe2342` are unrelated and are excluded from every target
below.

The six asset-specific modules currently contain about 3,680 physical lines:

| Module | Current lines | Main problem |
| --- | ---: | --- |
| `pipeline/assets.rs` | 2,198 | Orchestration, CSS provider, diagnostics, budgets, compression, binary handling, and 683 test lines are mixed together. |
| `pipeline/asset_budget.rs` | 776 | Repeats stat/read/reservation and observation concerns also present in `assets.rs`. |
| `pipeline/asset_boundary.rs` | 407 | The interface and 187 test lines are large relative to one two-permit blocking executor. |
| `pipeline/css_dependencies.rs` | 192 | Cohesive logic, but exposed beside rather than hidden inside the asset module. |
| `engine/asset_input.rs` | 83 | Cohesive exact-byte snapshot type; keep it. |
| `engine/asset_classifier.rs` | 24 | Cohesive classifier; keep it. |

### Required target

- Delete at least **1,800 net lines** from the feature implementation and its tests.
- Target **2,400 net deleted lines**; do not sacrifice a behavior or gate to reach it.
- Reduce the six-module asset cluster from about 3,680 lines to **2,300 lines or fewer**, including
  its tests.
- Reduce runtime implementation inside that cluster from about 2,640 lines to **1,450 lines or
  fewer**.
- Reduce asset interface/implementation tests to **650-750 lines**, plus only the independent
  end-to-end, cache, oracle, and performance gates that cross a real seam.
- Bring feature-only branch growth to **7,100 net lines or fewer**; **6,500** is the stretch target.

Generated lockfile changes, required product documentation, and the independent oracle do not count
as deletion opportunities. Moving code between files does not count as a reduction. Each phase must
report physical lines before and after, but behavior and architecture remain the acceptance criteria.

## 2. Behavior freeze

This refactor must preserve the following behavior exactly.

### Discovery and artifact model

1. The classifier recognizes the current CSS/preprocessor, wasm, and font extensions and leaves all
   other modules to Rolldown.
2. The engine captures each directly imported asset as one immutable byte snapshot with the
   fingerprint from the same read.
3. All reachable top-level stylesheets in a runtime group are first bundled as one CSS artifact.
4. If the CSS union fails for an eligible deterministic reason, each top-level sheet is retried
   separately and the result is disclosed as an upper bound through `imprecise_assets`.
5. Local font and wasm resources referenced by surviving CSS `url()` dependencies are counted once
   per canonical path. Remote, public-root, ambiguous, missing, and unreadable references retain
   their current diagnostic policy.
6. Each font/wasm file is a separate artifact. JavaScript, CSS, fonts, and wasm are compressed per
   artifact and only the resulting sizes are summed.
7. `asset_breakdown` contains at most one summed row for each current `AssetKind`.

### Freshness and resource safety

8. Every measured byte is bound to a fingerprint from that exact read. Conflicting observations of
   one path survive so the result cannot be considered fresh accidentally.
9. Failed or absent reads retain never-fresh evidence and request-local `asset_io` behavior.
10. Direct assets remain inside the graph's per-module, aggregate byte, and module-count limits.
11. CSS children and CSS-discovered resources remain inside the current build-wide input limits.
12. A CSS attempt keeps the 256-file/8-MiB tree bound. Union plus retries share the existing
    512-read/16-MiB work bound; retries may not reset it.
13. Asset processing remains globally two-wide with one eight-second deadline covering admission
    and execution. A timed-out or abandoned worker retains its permit until it actually finishes.
14. Deterministic limit failures retain the observations needed to expire after the offending file
    changes. Timeout, panic, engine loss, asset I/O, and compression remain non-durable.

### Result quality and consumers

15. `uncounted_assets` still makes a successful combined File Cost incomplete: it is a Floor and
    receives no budget verdict.
16. `imprecise_assets` remains a durable, cacheable upper bound but is not budgetable.
17. Import Cost continues to become Unmeasured when the whole asset-processing stage cannot produce
    a coherent result. File Cost continues to degrade to its existing per-import fallback for only
    the failed runtime group.
18. Import Cost, File Cost, Combined Import Cost, cache freshness, confidence, history, report, CLI,
    and extension behavior remain byte-for-byte and stage-for-stage equivalent.
19. The MessagePack protocol, `PROTOCOL_VERSION`, `ANALYZER_REVISION`, UI labels, compression levels,
    limits, and permit counts do not change during this refactor.

### Known limitations deliberately outside this refactor

- AC-04's direct JavaScript font/wasm loader model remains unresolved. Do not change
  `ModuleType::Empty`, emitted-reference JavaScript, side-effect import behavior, or the oracle
  policy here.
- Accepted D7, D8, D9, and D10 behavior remains accepted as currently documented.
- Do not add asset kinds, preprocessors, bare-`@import` resolution, inlining, source maps, or
  project-bundler configuration support.

Any patch that changes one of these items is a feature or bug-fix patch and must wait until this
refactor is complete.

## 3. Why the current module is shallow

Both production callers currently need to know too much:

```text
analyze.rs / file_size.rs
  -> process_assets_bounded(...)
  -> ProcessedAssets::total()
  -> asset_diagnostics(...)
  -> ProcessedAssets::freshness_fingerprints()
  -> ProcessedAssets::has_uncounted_assets()
  -> manually merge sizes, diagnostics, evidence, and quality
```

The implementation also carries the same concerns in parallel forms:

- four `TrackingProvider` constructors cover bounded/unbounded and synthetic/non-synthetic cases;
- `Option<AssetProcessingContext>` threads production safety through code that can bypass it only
  for tests;
- `ReadBudget` and `AssetProcessingContext` both reserve and reconcile metadata against actual
  reads;
- `ProcessedAssets` stores several correlated issue vectors and sets, then separate functions
  interpret them into diagnostics and quality;
- tests call private processing layers directly and then repeat the same behaviors through
  `analyze`, File Cost, and the service cache;
- `assets.rs` exposes helpers used only to keep those shallow tests working.

The deletion test is decisive: deleting any one of these shallow helpers mostly moves its knowledge
into callers. The replacement needs one deep module whose interface returns a complete measurement
and whose implementation owns all asset-specific knowledge.

## 4. Designs considered

### A. Minimal asset-only module

Expose one `asset_pipeline::measure(AssetInputs) -> AssetMeasurement` entry point. It is a large
improvement over the current interface and keeps asset quantity policy out of the module. However,
both callers would still separately minify/compress JavaScript, combine JavaScript and asset sizes,
and merge engine plus asset observations.

### B. Processor registry with an adapter per asset kind

Define processor traits and register CSS, font, and wasm adapters. This is rejected. CSS is many
inputs to one artifact with retry semantics; binaries are one input to one artifact. A common trait
would expose most of those differences in its interface, and there is no runtime variability that
needs a registry. It adds hypothetical seams and more code.

### C. Caller-oriented artifact measurement

Expose one `measure_artifact(&BundleArtifact)` entry point that owns JavaScript minification and
compression as well as the private asset pipeline. This matches the domain: ADR-0005 defines the
artifact boundary, and both Import Cost and File Cost need the same artifact measurement. Callers
retain only their genuinely different failure projections.

### Decision: C outside, A inside

Use `artifact_measurement` as the external seam and a minimal private asset module behind it. This
gives the callers the deepest interface while keeping the asset implementation independent of
whether its result becomes an Import Cost or a File Cost.

The filesystem and Lightning CSS are local-substitutable dependencies. Tests use real temporary
files and the pinned compiler; no filesystem or processor port is added. Compression failure
injection and tiny limits remain private test seams. The bounded executor stays private unless a
second real blocking workload can reuse it with fewer total lines; do not extract a generic executor
merely because the engine has a conceptually similar async boundary.

## 5. Target architecture

```text
pipeline/analyze.rs -----\
                         +--> artifact_measurement::measure(&BundleArtifact)
pipeline/file_size.rs --/             |
                                      +-- JavaScript minify/compress
                                      +-- assets::process(...)
                                             |
                                             +-- AssetSession (one ledger/evidence owner)
                                             +-- CSS union/retry + LightningProvider
                                             +-- CSS url() dependency classification
                                             +-- binary artifact compression
                                             +-- bounded executor/deadline
                                             +-- Issue finalization
```

### External interface

The exact field visibility may change during implementation, but callers should learn only this
shape:

```rust
pub(crate) fn measure(
    artifact: &BundleArtifact,
) -> Result<ArtifactMeasurement, ArtifactMeasurementFailure>;

pub(crate) struct ArtifactMeasurement {
    sizes: MeasuredSizes,
    javascript_minified_bytes: u64,
    asset_breakdown: Vec<AssetContribution>,
    diagnostics: Vec<ImportDiagnostic>,
    evidence: ReadEvidence,
    quality: MeasurementQuality,
}

pub(crate) struct ReadEvidence {
    fingerprints: Vec<FileFingerprint>,
    loaded_paths: Vec<PathBuf>,
}

pub(crate) struct MeasurementQuality {
    completeness: Completeness,
    precision: Precision,
    durability: Durability,
}

pub(crate) struct ArtifactMeasurementFailure {
    stage: &'static str,
    message: String,
    details: Vec<String>,
    evidence: ReadEvidence,
}
```

All fields should be private with narrow accessors needed by the two callers. The structural quality
types remain daemon-internal and project onto the existing diagnostics, `incomplete`, durability,
and budgetability behavior. They do not cross the wire in this refactor.

`javascript_minified_bytes` remains available because named-import tree-shakeability compares the
selected JavaScript chunk with the full-package JavaScript chunk; asset bytes must not enter that
ratio.

### Private implementation modules

Target layout:

```text
daemon/src/pipeline/
  artifact_measurement.rs       external seam and JavaScript artifact measurement
  assets/
    mod.rs                       private orchestration, outcome, issue finalization
    session.rs                   snapshots, budgets, deadline, and read evidence
    css.rs                       provider, union/retry, printing, resource discovery
    css_dependencies.rs          Lightning CSS dependency interpretation
    executor.rs                  two-wide bounded blocking execution
    tests.rs                     interface-level scenario matrix
```

Keep `engine/asset_input.rs` and `engine/asset_classifier.rs` where they are: classification and the
initial exact snapshot belong to the engine load seam.

Do not create separate `binary.rs`, `diagnostics.rs`, or `types.rs` modules unless their final
implementation is independently deep. A collection of tiny pass-through files improves neither
depth nor line count.

### `AssetSession`: one owner of reads and evidence

Replace `ReadBudget`, `ReadReservation`, `AssetProcessingContext`, and their overlapping observation
collections with one private `AssetSession`.

It owns:

- canonical-path aliases and immutable snapshots;
- exact and failed read evidence;
- graph-wide unique input files/bytes;
- build-wide CSS work reads/bytes;
- the absolute deadline;
- a small per-attempt CSS tree counter.

There are still three distinct limits because they protect different resources, but one read path
charges them:

```text
stat -> reserve global input -> reserve build work -> reserve attempt
     -> read once -> reconcile every reservation -> fingerprint exact bytes -> retain snapshot
```

The per-attempt counter may reset for a separate stylesheet attempt. The build-work counter,
deadline, snapshots, and evidence never reset across union and retries.

### One provider construction path

`LightningProvider` receives:

- one `Arc<AssetSession>`;
- the already captured top-level stylesheet snapshots;
- `Option<SyntheticEntry>` for a multi-root union.

Remove the bounded/unbounded constructor pairs. Production and tests both exercise the same code;
tests supply a private `TestPolicy` with small limits and a deterministic deadline.

### One issue finalizer

Replace the parallel `uncounted`, `failures`, `failed_paths`,
`stylesheets_measured_separately`, `css_dependency_failures`, and `non_durable_stages` state with an
internal `Issue` enum. One finalizer derives:

- diagnostics and their details;
- completeness, precision, and durability;
- uncounted-asset summaries;
- whether processing returns a partial measurement or a whole-stage failure.

Callers must not infer asset quality by inspecting individual internal collections. Existing stage
names remain the source used by cross-process durability and budgetability guards.

## 6. Test architecture: replace, do not layer

The new module's interface is the main test surface. Existing tests may be deleted only after their
behavior is mapped to a surviving row.

### One table-driven characterization matrix

Create a compact scenario builder and normalized assertion type. Paths are normalized relative to
the fixture root; diagnostics compare stage, stable message fragments, and details; fingerprints
compare path, length, metadata identity, and hash presence.

The matrix must cover:

1. no assets;
2. one stylesheet;
3. multiple stylesheets combined into one artifact;
4. shared `@import` and shared local font deduplication;
5. nested `@import` freshness;
6. local font and wasm `url()` resources;
7. remote, public-root, ambiguous, missing, and unreadable resources;
8. broken CSS with healthy per-sheet survivors;
9. all sheets uncounted;
10. per-attempt tree limit and build-wide retry-work limit;
11. aggregate source limit before read and growth reconciliation after read;
12. timeout, panic, asset I/O, and compression durability;
13. conflicting observations of one path;
14. direct binary snapshot reuse and per-artifact compression;
15. complete, Floor, and upper-bound quality projections.

### Tests that remain outside the module

Keep one focused test for each real seam:

- engine plugin classification, browser alias resolution, and direct-asset source admission;
- Import Cost headline plus `asset_breakdown` wire behavior;
- File Cost CSS union/deduplication and immediate invalidation;
- service cache reuse/expiry for one deterministic and one request-local asset outcome;
- the independent esbuild accuracy oracle;
- CSS-heavy and binary-heavy release performance/RSS gates;
- daemon/extension/CLI stage-coordination guards.

The cross-language stage guards are intentional guarded duplication across shipping boundaries.
Replacing them with a new protocol field or generated contract is outside this refactor and is
unlikely to be net-negative after migration and compatibility handling.

### Tests to delete or compact

- Delete processor tests that call `bundle_css`, `bundle_css_set`, or unbounded `process_assets`
  after the same behavior exists in the characterization matrix.
- Reduce the 683-line `assets.rs` test tail, 170-line budget test tail, and 187-line executor test
  tail to the matrix plus three executor properties: admission width, timeout retains permit, and
  panic isolation.
- Replace repeated package/workspace construction in `analyze.rs`, `service.rs`,
  `asset_freshness.rs`, and `asset_resource_limit.rs` with one asset fixture builder.
- Do not assert the same processor detail again at engine, pipeline, service, wire, and UI levels.
  Each higher-level test should assert only what that seam adds.
- Keep the oracle and performance fixtures independent; sharing them with implementation tests
  would weaken their value.

## 7. Ordered implementation commits

Every item below is a separate commit. Do not amend earlier feature commits. Run the exact full
`pnpm test` before each commit, as well as the narrow gate named for the phase.

### Commit 1: consolidate characterization fixtures

**Goal:** reduce test repetition before moving implementation.

- Add one normalized asset scenario builder.
- Convert existing processor tests into the table-driven matrix.
- Delete each old test in the same patch once its behavior maps to a matrix row.
- Keep production code unchanged.

**Target:** at least 250 net lines deleted.

### Commit 2: introduce the artifact-measurement seam

**Goal:** make both callers depend on one deep interface.

- Add `artifact_measurement::measure` as the only caller-facing entry point.
- Initially place the existing bounded asset implementation behind it.
- Move JavaScript minification/compression and artifact-size summation out of `analyze.rs` and
  `file_size.rs`.
- Return complete diagnostics, evidence, quality, and the JavaScript-only minified length.
- Delete the duplicated caller assembly immediately; do not leave a second path.

**Target:** 100-200 net lines deleted; no caller imports `pipeline::assets` afterward.

### Commit 3: remove the unbounded/test-only implementation

**Goal:** make tests and production use identical processing.

- Replace four provider constructors with one constructor.
- Make the processing session mandatory; remove `Option<AssetProcessingContext>`.
- Delete public `bundle_css`, `bundle_css_set`, and unbounded `process_assets` entry points.
- Supply small policies through private test helpers instead of bypassing admission and deadlines.
- Move the remaining freshness test through `artifact_measurement::measure`.

**Target:** 250-400 net lines deleted.

### Commit 4: unify the asset session and read ledgers

**Goal:** decide snapshot identity, resource charging, and evidence once.

- Introduce `AssetSession` and the small per-attempt CSS counter.
- Route top-level snapshots, `@import` reads, retry reads, and CSS `url()` reads through it.
- Preserve metadata-first admission and post-read growth reconciliation.
- Preserve conflicting observations and never-fresh failed paths.
- Delete `asset_budget.rs` and the `ReadBudget`/reservation implementation it replaces in
  `assets.rs`.

**Target:** 350-550 net lines deleted.

This is the highest-risk commit. Its narrow gates are the freshness, source-limit, retry-work,
timeout, and mutation-during-read scenarios.

### Commit 5: finalize issues and quality structurally

**Goal:** remove correlated state and repeated interpretation.

- Introduce the private `Issue` representation and one finalizer.
- Make `ArtifactMeasurement` carry ready-to-consume diagnostics, evidence, breakdown, and quality.
- Remove separate `asset_diagnostics`, `freshness_fingerprints`, `total`, and
  `has_uncounted_assets` calls from callers.
- Share the uncounted-asset name/byte summarizer with the engine adapter without changing either
  message.
- Remove unused asset helpers rather than preserving speculative interface.

**Target:** 200-350 net lines deleted.

### Commit 6: move the private implementation into focused files

**Goal:** restore locality without creating shallow modules.

- Delete `pipeline/assets.rs` after moving its surviving implementation behind the new module.
- Place CSS/provider logic, session logic, executor logic, and interface tests in the target layout.
- Keep the external module interface unchanged.
- Compact the executor tests to observable properties at its real internal seam.

**Target:** code move is net-zero by itself; cleanup in the same patch must delete at least 150 net
lines. No production file should exceed roughly 700 lines.

### Commit 7: remove superseded integration layers

**Goal:** retain coverage while removing repeated fixture and policy assertions.

- Audit every asset-related test against the matrix and real-seam list.
- Delete duplicate processor assertions from `analyze.rs` and `service.rs`.
- Shrink the one-test resource binary so it tests only process-level environment isolation and
  cache expiry; ledger details stay at the asset interface.
- Keep cache, wire, oracle, and performance behavior independently covered.

**Target:** 500-800 net lines deleted.

### Commit 8: architecture documentation and release artifact

**Goal:** record the new seam and ship the behavior-identical daemon.

- Update `bundler-architecture.md`, the asset design status, and the audit's AC-07 disposition.
- Do not rewrite the SRS's behavior because behavior did not change.
- State final before/after LOC and test counts honestly.
- Package win32-x64 and refresh its daemon integrity hash in a separate release commit if the
  repository's hash workflow requires it.
- Do not bump `ANALYZER_REVISION` or `PROTOCOL_VERSION`.

## 8. Verification gates

### Before the first implementation commit

- Record `8fe2342` as the behavior reference.
- Run the existing full suite once and retain the accuracy/performance output as the baseline.
- Record current line counts by category. Exclude `.claude`/`CLAUDE.md` orchestration changes.

### For every code commit

1. Run the narrow Rust test target for the changed seam.
2. Run exact `pnpm test` and require every TypeScript, script, and Rust suite to pass.
3. Run `pnpm lint` and `pnpm check`.
4. Run `cargo clippy --workspace --all-targets --locked -- -D warnings`.
5. Run `git diff --check`.
6. Compare normalized characterization output with the frozen baseline.
7. Confirm the patch contains no compatibility implementation left beside its replacement.
8. Commit before beginning the next phase; never amend a prior issue/refactor commit.

### Final gates

- `pnpm test`
- `pnpm lint`
- `pnpm check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `pnpm test:accuracy`
- `pnpm test:performance`
- the ignored real-fixture `candidate_performance` asset cases when the fixture workspace is
  prepared;
- `pnpm package:win32-x64`, VSIX size/integrity verification, and daemon launch verification;
- `git diff --check`;
- a final grep proving neither caller imports private asset internals;
- final LOC accounting against both `8fe2342` and `b29c329`.

## 9. Stop conditions

Stop a phase and reassess instead of forcing the target if:

- a normalized size, contribution, diagnostic stage/detail, fingerprint, loaded path, cache status,
  completeness flag, or budget verdict changes;
- a retry receives a fresh build-work budget or deadline;
- a test needs a new public hook solely to inspect an implementation detail;
- a proposed trait has only one adapter;
- an extraction moves code but does not remove knowledge from callers;
- a shared engine/asset executor abstraction increases total code or weakens timeout semantics;
- the implementation requires an analyzer/protocol revision;
- LOC is reduced by deleting an independent oracle, cache gate, performance gate, or required
  product documentation.

If the minimum 1,800-line deletion cannot be reached without one of these failures, report the
measured lower reduction and the code that remains load-bearing. Do not hide the miss by weakening
coverage or moving lines out of the counted paths.

## 10. Definition of done

The refactor is complete only when all of the following are true:

- Import Cost and File Cost call one `artifact_measurement` interface.
- No caller knows about CSS providers, retry strategy, asset budgets, deadline implementation,
  compression loops, or issue collections.
- There is one production asset-processing path and no optional safety context.
- There is one implementation of asset stat/read reservation and reconciliation.
- There is one owner of asset evidence and one finalizer for diagnostics/quality.
- The engine still owns classification and initial exact snapshots.
- Every frozen behavior and final verification gate passes.
- AC-04 and the accepted D7-D10 limitations remain separately visible and unchanged.
- At least 1,800 net lines are deleted, with a documented goal of 2,400.
- The final architecture documentation explains the seam and reports the actual reduction.

