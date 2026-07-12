# Bundler Redesign — Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: this repo's `CLAUDE.md` mandates `subagent-driven-development`. Implement inline; spend subagent tokens on independent review of the risky commits (Tasks 1, 4, 5, 8, 12). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Ship `bundler-redesign` with no fabricated numbers, no undetectable regressions, and no panic that can take down a batch.

**Architecture:** The Rolldown cutover is architecturally complete and is not revisited. This plan closes the *edges*: it contains panics at the engine boundary, turns on the gates that have never run, makes the entry module known to Rolldown, stops the product reporting quantities it cannot compute, and deletes the hand-written logic upstream already does better.

**Tech Stack:** Rust daemon (`daemon/`), Rolldown `=1.1.5` on OXC `=0.139.0`, `oxc_resolver =11.23.0`; TypeScript extension host (`extension/`); pnpm; lefthook; GitHub Actions.

**Source spec:** [`../specs/2026-07-12-bundler-redesign-release-plan.md`](../specs/2026-07-12-bundler-redesign-release-plan.md) — the scope and its rationale. **Decisions:** [ADR-0001](../../adr/0001-measure-a-neutral-build.md)–[ADR-0005](../../adr/0005-a-runtime-is-an-artifact-boundary.md). **Vocabulary:** [`CONTEXT.md`](../../../CONTEXT.md). **Design amendments:** I16–I23 in [`../specs/2026-07-10-bundler-redesign-design.md`](../specs/2026-07-10-bundler-redesign-design.md). The findings record is [`../specs/2026-07-12-bundler-redesign-release-review.md`](../specs/2026-07-12-bundler-redesign-release-review.md) (superseded as a plan).

---

## Global Constraints

- **Branch:** all work lands on `bundler-redesign`. Never commit to `main`.
- **Commits:** one commit per *logically-coherent change*, **not** one per task or step. Tasks that share a commit say so explicitly. This overrides the plan-template default (`CLAUDE.md` → Git Expectations).
- **Line endings:** LF only. **Package manager:** `pnpm` only.
- **Compiler-stack pins are exact:** `rolldown =1.1.5`, all OXC crates `=0.139.0`, `oxc_resolver =11.23.0`. **This plan adds exactly one new crate — `fast-glob` (Task 4) — and it is exact-pinned into the compiler stack**, per ADR-0002. No other new dependency. Do not assert any *other* dependency's version in a test.
- **Testing policy:** Logic / Drift / Property / Guard only. No Echo tests — never write a test whose expected value you typed by hand out of the file under test (`CLAUDE.md` → Testing Policy).
- **SRS:** if behavior diverges from `docs/ImportLens-SRS.md`, update the SRS **in the same task**. Several tasks below change user-visible behavior; each says what the SRS owes.
- **`ANALYZER_REVISION` is bumped exactly once, in Task 14**, after every measurement-affecting change has landed. Do not bump it per-task.
- **Verification (full set, before completion):**
  ```powershell
  pnpm check
  pnpm test
  cargo fmt --check
  pnpm package:win32-x64
  ```

### Verified facts (do not re-derive; checked against the vendored crates in `~/.cargo/registry`)

| Thing                                   | Truth                                                                                                                                                                                                                                                                                                                |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `futures_util::FutureExt::catch_unwind` | **Available with no new dependency.** `futures-util ^0.3` is already a daemon dep with the `std` feature (`daemon/Cargo.toml:15`), which is what gates `catch_unwind`.                                                                                                                                               |
| `rolldown_common::ModuleInfo` (1.1.5)   | Carries `code`, `id`, `is_entry`, `importers`, `dynamic_importers`, `imported_ids`, `dynamically_imported_ids`, `exports`, `input_format`. **No side-effect field.** `DeterminedSideEffects` lives on internal module types and reaches no output type. Rolldown therefore *cannot* supply the Side-Effectful badge. |
| `HookBuildEndArgs` (1.1.5)              | Carries `errors: &Vec<BuildDiagnostic>` and `cwd` only. **Warnings from a failed build are unrecoverable.** Do not go looking for them.                                                                                                                                                                              |
| `fast-glob`                             | Already in `Cargo.lock` transitively; `rolldown_utils::pattern_filter` calls `fast_glob::glob_match`. Adding it as a direct dep introduces **no new supply-chain surface**.                                                                                                                                          |
| `HookResolveIdOutput`                   | Carries `package_json_path: Option<String>`. For a plugin-resolved id, Rolldown builds `ResolvedId.package_json` **only** from this field.                                                                                                                                                                           |
| `ci.yml`                                | Already sets `run_candidate_packages: true`. `run_performance` is `true` **only** in `build.yml`'s `workflow_dispatch`. This is why the §10.6 gates have never run on a PR.                                                                                                                                          |

### Ordering rationale

**The instruments come before the changes they must judge.** Tasks 3, 4 and 8 all move real-package bytes or badges, and today nothing anywhere would detect a wrong move — the accuracy oracle checks *bytes*, never *claims*. Task 2 builds the missing instrument and turns on the dead gate. It is the largest task and the one with the least visible payoff, and everything downstream is unverifiable without it.

Task 5 (Unmeasured) **creates** the CSS regression that Task 6 fixes. They may not be separated across a release.

Task 14 (packaging) is last because every task above changes the daemon binary.

---

## Task 1: Contain panics at the engine boundary

**Files:**
- Modify: `daemon/src/engine/boundary.rs`
- Test: `daemon/tests/panic_isolation.rs` (exists — extend it)

**Interfaces:**
- Produces: `boundary::bundle_sync` and `boundary::enumerate_exports_sync` keep their signatures but **never panic**; a panicking build returns `Err(BundleFailure { stage: "panic", .. })`.

**Why:** `run_on_engine` blocks on `receiver.recv().expect("the engine runtime should always reply")` (`boundary.rs:86-89`). If a Rolldown/OXC build panics, Tokio catches the task panic and drops the sender, `recv()` returns `Err`, and **the calling analysis thread panics**. It unwinds through `thread::scope` (`scheduling.rs:34-50`) → `drain_classified` → `handle_batch`, and `ipc/server.rs:1038-1041` turns the `JoinError` into a batch-level protocol error. One pathological package destroys an entire batch — **including every import already answered from cache**. Making the release profile unwind (`6707baf`) made the daemon *survive*; it did not *isolate*. The interactive paths (`Batch`, `AnalyzeDocument`, `FileSize`, `AnalyzePackageJson`) have zero engine panic isolation.

**Companion:** `IN_FLIGHT.fetch_sub` (`boundary.rs:71`) is skipped on unwind, so the counter leaks and `PEAK_IN_FLIGHT` latches an inflated value. `peak_in_flight()` is the **only** assertion of the §9 two-build invariant (`daemon/tests/engine_boundary.rs:69`), so after two panicking builds the daemon's sole concurrency check reports garbage.

- [ ] **Step 1: Write the failing test**

Add to `daemon/tests/panic_isolation.rs`:

```rust
/// A panic inside a build must degrade that one build, not the caller. Before the
/// `catch_unwind`, `run_on_engine`'s `recv().expect(...)` panicked the *calling*
/// thread, which unwound through `thread::scope` and failed the whole batch.
#[test]
fn engine_panic_becomes_a_typed_failure_not_a_caller_panic() {
    let failure = import_lens_daemon::engine::boundary::bundle_sync_for_test_panic()
        .expect_err("a panicking build must return a failure, not unwind the caller");

    assert_eq!(failure.stage, "panic");
    assert!(
        failure.message.contains("engine build panicked"),
        "message should name the panic: {}",
        failure.message
    );
}

/// The in-flight counter must not leak when a build unwinds: `peak_in_flight()` is the
/// only assertion of the §9 two-build invariant, so a latched counter silently disables
/// the daemon's sole concurrency check.
#[test]
fn a_panicking_build_does_not_leak_the_in_flight_counter() {
    let before = import_lens_daemon::engine::boundary::peak_in_flight();

    for _ in 0..3 {
        let _ = import_lens_daemon::engine::boundary::bundle_sync_for_test_panic();
    }

    assert_eq!(
        import_lens_daemon::engine::boundary::peak_in_flight(),
        before.max(1),
        "peak must not climb with each panicking build"
    );
}
```

Add the test-only entry point to `daemon/src/engine/boundary.rs`:

```rust
/// Drives a build future that panics, through the real permit/runtime path. Exists so
/// the isolation guarantee is tested against the boundary that ships, not a mock.
#[doc(hidden)]
pub fn bundle_sync_for_test_panic() -> Result<BundleArtifact, BundleFailure> {
    run_on_engine(async { panic!("synthetic engine panic") })
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p import-lens-daemon --test panic_isolation`
Expected: the process aborts or the test panics with `the engine runtime should always reply` — **not** a clean assertion failure. That *is* the bug.

- [ ] **Step 3: Rewrite the boundary**

In `daemon/src/engine/boundary.rs`, add the imports:

```rust
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
```

Replace `with_permit` (lines 62-73) with a drop-guarded version:

```rust
/// Decrements on drop, so an unwind cannot leak the counter. The semaphore permit
/// already works this way; the counters did not.
struct InFlight;

impl InFlight {
    fn enter() -> Self {
        STARTED.fetch_add(1, Ordering::Relaxed);
        let current = IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        PEAK_IN_FLIGHT.fetch_max(current, Ordering::Relaxed);
        Self
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn with_permit<T>(work: impl Future<Output = T>) -> T {
    let _permit = PERMITS
        .acquire()
        .await
        .expect("engine permit semaphore is never closed");
    let _in_flight = InFlight::enter();
    work.await
}
```

Replace `run_on_engine` (lines 81-89). It now requires the future to produce a `Result<_, BundleFailure>`, which both callers already do:

```rust
/// Submit work to the engine runtime and block the calling thread until it completes.
///
/// The build future is wrapped in `catch_unwind`: a Rolldown or OXC panic becomes a
/// typed `BundleFailure` for *this* import, and the §12 fallback arm handles it. Before
/// this, a panicking task dropped the channel sender, `recv()` returned `Err`, and the
/// `expect` panicked the calling analysis thread — destroying the whole batch, including
/// every import already answered from cache.
fn run_on_engine<T: Send + 'static>(
    work: impl Future<Output = Result<T, BundleFailure>> + Send + 'static,
) -> Result<T, BundleFailure> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    engine_runtime().spawn(async move {
        let outcome = with_permit(AssertUnwindSafe(work).catch_unwind())
            .await
            .unwrap_or_else(|payload| Err(panic_failure(&payload)));
        let _ = sender.send(outcome);
    });

    // The sender is dropped without a send only if the engine runtime itself is gone.
    // That is not recoverable, but it is still THIS import's failure, not the calling
    // thread's panic -- which is the entire point of this function.
    receiver.recv().unwrap_or_else(|_| {
        Err(BundleFailure {
            stage: "panic".to_owned(),
            message: "the engine runtime dropped the build without replying".to_owned(),
            diagnostics: Vec::new(),
            loaded_paths: Vec::new(),
        })
    })
}

/// Rust panic payloads are `&str` for a literal `panic!` and `String` for a formatted one;
/// anything else is opaque. Name what we can and stay honest about the rest.
fn panic_failure(payload: &(dyn std::any::Any + Send)) -> BundleFailure {
    let detail = payload
        .downcast_ref::<&'static str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());

    BundleFailure {
        stage: "panic".to_owned(),
        message: format!("engine build panicked: {detail}"),
        diagnostics: Vec::new(),
        loaded_paths: Vec::new(),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p import-lens-daemon --test panic_isolation --test engine_boundary`
Expected: PASS. `engine_boundary`'s existing `peak_in_flight` assertion must still hold.

- [ ] **Step 5: Full daemon suite**

Run: `cargo test -p import-lens-daemon --locked`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/engine/boundary.rs daemon/tests/panic_isolation.rs
git commit
```
Message: `fix(daemon): isolate engine panics to one import` — body must explain that a panicking build previously destroyed an entire batch including cached hits, and that the in-flight counter leaked on unwind, disabling the only §9 concurrency assertion.

---

## Task 2: Turn on the gates, and build the instrument that does not exist

**Files:**
- Modify: `.github/workflows/validate.yml:143-150`
- Create: `daemon/tests/candidate_badges.rs`
- Modify: `daemon/tests/common/mod.rs` (expose a pipeline-level fixture helper)
- Modify: `scripts/prepare-candidate-fixtures.mjs` (add a CSS-shipping package)

**Interfaces:**
- Produces: `candidate_badges.rs` — asserts `side_effects`, `truly_treeshakeable` and `confidence` for every pinned real package. Tasks 3, 4 and 8 depend on it existing and being green *before* they land.

**Why (two independent holes):**

1. **The §10.6 performance and memory gates are dead code.** `daemon/tests/candidate_performance.rs:92,130` are `#[ignore]`d and need `IMPORT_LENS_FIXTURES_WORKSPACE`; **nothing invokes them.** The trap is that a perf gate *appears* to run: `validate.yml:150` calls `pnpm test:performance` → `package.json:221` → `cargo test … --test performance`, the pre-existing **legacy** suite over synthetic fixtures, a different file. And `run_performance` is `true` only in `build.yml`'s `workflow_dispatch`, so even that never fires on a PR. A Rolldown bump that doubles cold p95 or blows the 20-import RSS ceiling ships green.
2. **No real package's *claims* are baselined.** Nothing in `scripts/` mentions `truly_treeshakeable`. `candidate_packages.rs` works at engine level and never produces an `ImportResult`. The accuracy oracle checks bytes, never badges. Task 4 will flip `truly_treeshakeable` and confidence across every package declaring an array `sideEffects` — a large fraction of real packages — and today nothing would notice if it flipped wrongly.

- [ ] **Step 1: Wire the perf gate into CI**

In `.github/workflows/validate.yml`, immediately after the `Run real-package qualification tests` step (line 143-144), add:

```yaml
      # The §10.6 absolute performance and memory gates. These are `#[ignore]`d and
      # need the same installed fixtures as the step above -- and until now nothing
      # invoked them, so the suite written to protect this engine had never run. The
      # `Run performance smoke test` step below is the LEGACY synthetic suite, a
      # different file; it is not a substitute.
      - name: Run real-package performance gates
        if: ${{ inputs.run_candidate_packages }}
        run: cargo test -p import-lens-daemon --release --locked --test candidate_performance -- --ignored --nocapture
```

Gated on `run_candidate_packages` (not `run_performance`) deliberately: `ci.yml:23` already sets it `true`, so this runs on every PR, which is the whole point. `--release` matters — a debug-build timing gate is meaningless.

- [ ] **Step 2: Add a CSS-shipping fixture**

The §10.3 real-package set is **entirely pure JavaScript**, which is exactly why the Task 6 asset defect survived qualification. In `scripts/prepare-candidate-fixtures.mjs`, add `react-toastify` (small, ships CSS, stable) to the pinned fixture set alongside `css-tree`, `date-fns`, `lodash`, `lodash-es`, `zod`, `react`, `uuid`. Regenerate the committed fixture lockfile.

- [ ] **Step 3: Write the badge baseline (it is the instrument, so it must be able to fail)**

Create `daemon/tests/candidate_badges.rs`. This is a **Logic** test: real packages through the pipeline to outputs. It runs the *pipeline* (`analyze_import`), not the engine, because `ImportResult` is where badges live.

```rust
//! Real-package badge baseline (spec §10.6 amendment I23).
//!
//! The accuracy oracle checks BYTES. Nothing checked CLAIMS. `truly_treeshakeable` moved
//! most visibly at cutover and had no real-package ground truth anywhere: `scripts/` never
//! mentions it, and `candidate_packages.rs` stops at the engine boundary and never builds
//! an `ImportResult`. This file is that ground truth.
//!
//! Every row is `#[ignore]`d and needs `IMPORT_LENS_FIXTURES_WORKSPACE`; CI installs the
//! fixtures and runs it with `-- --ignored`.

mod common;

use import_lens_daemon::ipc::protocol::ImportRuntime;

struct BadgeExpectation {
    package: &'static str,
    named: &'static [&'static str],
    side_effects: bool,
    truly_treeshakeable: bool,
    confidence: &'static str,
}

/// The expectations are derived from what each package DECLARES, not copied from a run.
/// `date-fns` declares `"sideEffects": false` and is a wide barrel, so a single named
/// import must be both non-side-effectful and truly tree-shakeable. `react` is CJS, so it
/// cannot be. Add a row when a fixture is added; a new package with no row is a gap.
const EXPECTATIONS: &[BadgeExpectation] = &[
    BadgeExpectation {
        package: "date-fns",
        named: &["format"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: "high",
    },
    BadgeExpectation {
        package: "zod",
        named: &["z"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: "high",
    },
    BadgeExpectation {
        package: "lodash-es",
        named: &["debounce"],
        side_effects: false,
        truly_treeshakeable: true,
        confidence: "high",
    },
];

#[test]
#[ignore = "needs IMPORT_LENS_FIXTURES_WORKSPACE"]
fn real_package_badges_hold() {
    let workspace = common::fixtures_workspace();

    for expected in EXPECTATIONS {
        let result = common::analyze_named_import(
            &workspace,
            expected.package,
            expected.named,
            ImportRuntime::Component,
        )
        .unwrap_or_else(|error| panic!("{} failed to analyze: {error}", expected.package));

        assert_eq!(
            result.side_effects, expected.side_effects,
            "{} side_effects",
            expected.package
        );
        assert_eq!(
            result.truly_treeshakeable, expected.truly_treeshakeable,
            "{} truly_treeshakeable",
            expected.package
        );
        assert_eq!(
            result.confidence, expected.confidence,
            "{} confidence (reasons: {:?})",
            expected.package, result.confidence_reasons
        );
    }
}

/// Anti-vacuity Guard, mirroring `candidate_packages::dangling_binding_gate_is_not_vacuous`.
/// A baseline that cannot fail is not a baseline. If `truly_treeshakeable` were hardwired
/// to `true`, every row above would still pass — so prove the assertion can fire by feeding
/// it a package that must NOT be truly tree-shakeable.
#[test]
#[ignore = "needs IMPORT_LENS_FIXTURES_WORKSPACE"]
fn the_badge_gate_is_not_vacuous() {
    let workspace = common::fixtures_workspace();

    // `react` is CJS: the full-package comparison cannot run, so the flag must be false.
    let result = common::analyze_named_import(&workspace, "react", &["useState"], ImportRuntime::Component)
        .expect("react should analyze");

    assert!(
        !result.truly_treeshakeable,
        "a CJS package must never be reported truly tree-shakeable; if this passes, the \
         gate above is asserting nothing"
    );
}
```

- [ ] **Step 4: Add the pipeline-level fixture helper**

`daemon/tests/common/mod.rs` currently serves engine-level tests. Add `fixtures_workspace()` (returning the `IMPORT_LENS_FIXTURES_WORKSPACE` path, **panicking** if absent — a skipped gate is a dark gate) and `analyze_named_import(workspace, package, named, runtime) -> Result<ImportResult, String>`, which builds an `AnalysisContext` + `ImportRequest` and calls the pipeline's `analyze_import`. Mirror the existing construction in `daemon/tests/analyze.rs`.

- [ ] **Step 5: Run it against fixtures**

```powershell
node scripts/prepare-candidate-fixtures.mjs "$env:TEMP/candidate-fixtures"
$env:IMPORT_LENS_FIXTURES_WORKSPACE="<path printed above>"
cargo test -p import-lens-daemon --release --locked --test candidate_badges -- --ignored --nocapture
cargo test -p import-lens-daemon --release --locked --test candidate_performance -- --ignored --nocapture
```
Expected: both PASS. If `candidate_performance` fails, **stop** — it has never run, so a failure here is a real, previously-invisible regression and must be understood before anything else lands.

- [ ] **Step 6: Prove the perf gate is wired, not just present**

Temporarily lower one §10.6 constant (e.g. the 500 ms cold p95) to `1`, push, and confirm CI goes red. Revert. A gate nobody has seen fail is a gate nobody knows runs — that is the exact failure this task exists to fix.

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/validate.yml daemon/tests/candidate_badges.rs daemon/tests/common/mod.rs scripts/prepare-candidate-fixtures.mjs
git commit
```
Message: `test(daemon): run the §10.6 gates and baseline real-package badges` — body must state that `candidate_performance` had never executed in CI, that `pnpm test:performance` is a different (legacy) suite, and that no real package's `truly_treeshakeable` was asserted anywhere before this.

---

## Task 3: Give the entry module its package

**Files:**
- Modify: `daemon/src/engine/plugin.rs:189-191`
- Modify: `daemon/tests/candidate_matrix.rs` (add the production-shaped row)

**Interfaces:**
- Consumes: nothing. **Produces:** the entry module's `ResolvedId.package_json` is populated, so Rolldown applies the package's `sideEffects` to it.

**Why:** `plugin.rs:189-191` returns `HookResolveIdOutput::from_id(target)`. That type carries `package_json_path: Option<String>`, and for a plugin-resolved id Rolldown builds `ResolvedId.package_json` **only** from that field. So the **entry module** gets `package_json: None` and its side-effect classification falls back to pure source analysis. Every *transitive* module is resolved by Rolldown itself and is fine. The entry is the sole hole — and the entry is the file every measurement is rooted at.

**Blast radius, stated honestly:** the entry is always retained (it provides the requested export), so nothing needed can be dropped. What changes is *statement* retention **within the entry file**: a package declaring `"sideEffects": false` whose entry has top-level statements source analysis cannot prove pure (`Object.freeze(...)`, a prototype patch, a self-registration call) keeps those statements and everything they reach. Rollup and webpack drop them. The reported size is inflated.

**Why nothing caught it:** every side-effects matrix row (`candidate_matrix.rs:950-961`) builds its fixture with `write_side_effect_package`, which writes a **workspace-root `entry.js`** doing `import 'testpkg'`. That makes `testpkg` *transitive* — correctly metadata-bearing. Production is the opposite shape: the user imports `date-fns`, so `entry_path` **is** `node_modules/date-fns/…`, resolved by the plugin, on the exact path that loses its metadata. The seven rows proving "Rolldown owns `sideEffects`" all exercise the one code path production never takes.

- [ ] **Step 1: Write the failing row — the production shape**

In `daemon/tests/candidate_matrix.rs`, add:

```rust
/// The production shape: the measured entry IS a file inside a `node_modules` package,
/// resolved by our plugin -- not a workspace file importing that package. Rows 38-44 all
/// use the latter shape, which is why they never saw that a plugin-resolved entry loses
/// its `package.json` and falls back to source analysis for side effects.
///
/// The package declares `"sideEffects": false`, so Rolldown must drop the impure-looking
/// top-level statement and everything it reaches.
#[test]
fn matrix_46_entry_inside_node_modules_honors_package_side_effects() {
    let fixture = write_node_modules_package(
        "sefalse",
        r#"{ "name": "sefalse", "version": "1.0.0", "sideEffects": false }"#,
        // `Object.freeze` on a module-level object is the canonical statement source
        // analysis cannot prove pure. `unused` is never imported, so with the manifest
        // in hand Rolldown drops both it and the freeze call.
        r#"
        export const used = 1;
        const unused = { a: 1 };
        Object.freeze(unused);
        export { unused };
        "#,
    );

    let artifact = bundle_ok(BundleRequest {
        entries: vec![BundleEntry {
            entry_path: fixture.entry_path.clone(),
            package_root: fixture.package_root.clone(),
            selection: named(&["used"]),
            reported_side_effects: SideEffectsMode::False,
        }],
        runtime: ImportRuntime::Component,
        purpose: BundlePurpose::Import,
    });

    assert!(
        !artifact.code.contains("Object.freeze"),
        "a `sideEffects: false` package's impure-looking entry statement must be dropped; \
         it survives when the entry's package.json never reaches Rolldown:\n{}",
        artifact.code
    );
}
```

Add the `write_node_modules_package` helper alongside `write_side_effect_package`: it must create `<workspace>/node_modules/<name>/package.json` + `index.js` and return the **entry path inside `node_modules`** and its `package_root`.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p import-lens-daemon --test candidate_matrix matrix_46`
Expected: FAIL — `Object.freeze` is present in the output. This is the defect, reproduced.

- [ ] **Step 3: Supply the manifest path**

In `daemon/src/engine/plugin.rs`, at the `resolve_id` return (line ~189):

```rust
        // §7.4/I17: Rolldown builds `ResolvedId.package_json` for a plugin-resolved id
        // ONLY from this field. Without it the entry -- the file every measurement is
        // rooted at -- gets `package_json: None` and its side-effect classification falls
        // back to source analysis, while every transitive module (resolved by Rolldown
        // itself) gets its metadata. This is metadata SUPPLY, not a semantic override:
        // Rolldown still decides retention, we just stop hiding an input from it.
        Ok(Some(HookResolveIdOutput {
            package_json_path: Some(
                entry
                    .package_root
                    .join("package.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..HookResolveIdOutput::from_id(target)
        }))
```

**Verified against the vendored crate:** `HookResolveIdOutput` (1.1.5) has public fields `id`, `external`, `normalize_external_id`, `side_effects`, `package_json_path`, and `from_id` is its **only** constructor — there is no `with_package_json_path` builder, so struct-update syntax is the form. Leave `side_effects: None`: §7.4 forbids the plugin overriding Rolldown's side-effect decision, and we are supplying *metadata*, not a decision.

- [ ] **Step 4: Run the row**

Run: `cargo test -p import-lens-daemon --test candidate_matrix matrix_46`
Expected: PASS.

- [ ] **Step 5: Re-run the baselines — sizes move here**

```powershell
cargo test -p import-lens-daemon --release --locked --test candidate_badges --test candidate_packages -- --ignored
node scripts/accuracy-compare.mjs
```
Expected: green. Sizes for packages with impure-looking entry statements go **down** (closer to esbuild). If an accuracy delta *worsens*, stop and investigate — this fix should only move us toward the oracle.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/engine/plugin.rs daemon/tests/candidate_matrix.rs
git commit
```
Message: `fix(daemon): give the entry module its package manifest` — body must explain that the entry was the only module Rolldown could not classify, that reported sizes were inflated for `sideEffects: false` packages, and that every existing side-effects row tested the transitive shape production never takes.

---

## Task 4: Array `sideEffects` — the badge is about the import, and the matcher is Rolldown's

**Files:**
- Modify: `daemon/src/pipeline/analyze.rs:314-318`
- Modify: `daemon/src/pipeline/resolver.rs:747-766` (delete the hand-rolled matcher)
- Modify: `daemon/Cargo.toml` (add `fast-glob`, exact-pinned)
- Modify: `scripts/compiler-stack.config.mjs` + `scripts/compiler-stack.fingerprint.json`
- Test: `daemon/tests/analyze.rs`

**Interfaces:**
- Consumes: Task 2's badge baseline (**must be green before this lands** — this task moves badges on real packages).
- Produces: `side_effects` is `entry_matches` for the array form; `SideEffectsMode::matching_paths` matches via `fast_glob::glob_match`.

**Why (two defects, one commit — they are the same change):**

1. `analyze.rs:318` reads `side_effects_mode.has_side_effects() || side_effects_mode.is_array()`. `has_side_effects()` **already answers correctly for arrays** (`resolver.rs:36-41` consults the matched patterns). The `|| is_array()` overrides that correct answer with `true`, unconditionally. `analyze.rs:339` gates the full-package comparison on `!side_effects`, so `truly_treeshakeable` becomes `false` **by construction** — the comparison build never runs — and `adapter.rs:214-236` pushes a diagnostic dropping confidence to Medium. `"sideEffects": ["**/*.css"]` is an everyday declaration: **every such package is reported side-effectful, never truly tree-shakeable, and never high-confidence** — even when Rolldown demonstrably tree-shook it and the measured size proves it. The code's justification ("glob matching unavailable from public bundler metadata") was **retracted by the §10.7 divergence-1 amendment**; matrix rows 42/43 prove Rolldown matches string and array globs correctly on Windows. The justification went; the conservatism stayed.
2. The matcher those patterns go through is **hand-rolled** — `resolver.rs:747-766` implements its own brace expansion, path-component matching and segment matching, and `daemon/Cargo.toml` has **no glob crate**. Meanwhile `fast-glob` — the matcher Rolldown itself uses (`rolldown_utils::pattern_filter` calls `fast_glob::glob_match`) — is already in `Cargo.lock`. Two glob engines reading one `sideEffects` array can disagree, and then we label a file the opposite way from how Rolldown treated it. This was harmless only while `|| is_array()` discarded the answer. Defect 1's fix makes it **load-bearing**, so it cannot be deferred past it (ADR-0002).

- [ ] **Step 1: Write the failing test**

In `daemon/tests/analyze.rs` (the existing side-effects tests at 2009-2046 cover only the string form):

```rust
/// A CSS-only `sideEffects` array says nothing about a JavaScript import. Reporting it as
/// side-effectful forced `truly_treeshakeable` to false BY CONSTRUCTION -- the comparison
/// build was gated off -- and dropped confidence to medium, on one of the most common
/// declarations in the ecosystem.
#[test]
fn array_side_effects_that_do_not_match_the_entry_report_no_side_effects() {
    let fixture = write_package(
        r#"{ "name": "cssonly", "version": "1.0.0", "sideEffects": ["**/*.css"] }"#,
        "export const a = 1;\nexport const b = 2;\n",
    );

    let result = analyze_named(&fixture, &["a"]).expect("should analyze");

    assert!(
        !result.side_effects,
        "a `**/*.css` pattern must not make a JS entry side-effectful"
    );
    assert!(
        result.truly_treeshakeable,
        "the full-package comparison must actually run when the entry is not side-effectful"
    );
    assert_eq!(result.confidence, "high");
}

/// The other direction, so the fix is not "always report false". An entry the package
/// itself lists as side-effectful must still say so.
#[test]
fn array_side_effects_that_match_the_entry_report_side_effects() {
    let fixture = write_package(
        r#"{ "name": "impure", "version": "1.0.0", "sideEffects": ["index.js"] }"#,
        "export const a = 1;\n",
    );

    let result = analyze_named(&fixture, &["a"]).expect("should analyze");

    assert!(result.side_effects, "the entry matches the pattern");
    assert!(!result.truly_treeshakeable);
}
```

- [ ] **Step 2: Run and watch the first one fail**

Run: `cargo test -p import-lens-daemon --test analyze array_side_effects`
Expected: `array_side_effects_that_do_not_match_the_entry_report_no_side_effects` FAILS (`side_effects` is `true`). The second already passes — it is the regression guard for the fix.

- [ ] **Step 3: Drop the override**

`daemon/src/pipeline/analyze.rs`, replacing lines 314-318:

```rust
    // §7.4/I18: Side-Effectful is a property of THE IMPORT, not the package. For the array
    // form `has_side_effects()` already consults the matched patterns and answers correctly
    // -- a `**/*.css` rule says nothing about a JS entry. The `|| is_array()` that used to
    // sit here overrode that correct answer with `true`, which gated off the full-package
    // comparison below and forced `truly_treeshakeable` to false by construction.
    let side_effects = side_effects_mode.has_side_effects();
```

- [ ] **Step 4: Add `fast-glob`, exact-pinned into the compiler stack**

`daemon/Cargo.toml`, in `[dependencies]` (alphabetical, after `brotli`):

```toml
# Rolldown's own side-effects glob matcher (`rolldown_utils::pattern_filter`). Exact-pinned
# and fingerprinted with the rest of the compiler stack (ADR-0002): its entire purpose is to
# agree with Rolldown, and a version skew would break that agreement silently.
fast-glob = "=1.0.1"
```

**Verified:** `1.0.1` is what `Cargo.lock` already resolves for `fast-glob` (pulled in transitively by `rolldown_utils`), so this adds **no new supply-chain surface** — it promotes an existing transitive dep to a direct, pinned one. Then add `fast-glob` to the compiler-stack package list in `scripts/compiler-stack.config.mjs` and regenerate `scripts/compiler-stack.fingerprint.json` (52 packages → 53).

- [ ] **Step 5: Delete the hand-rolled matcher**

In `daemon/src/pipeline/resolver.rs`, delete `side_effects_pattern_matches`, `normalize_side_effect_pattern`, `expand_brace_patterns`, `path_components_match` and `segment_pattern_matches`, and route through Rolldown's matcher:

```rust
/// The matcher is Rolldown's own (`fast_glob`), not a lookalike. Two glob engines reading
/// one `sideEffects` array can disagree, and then we would label a file the opposite way
/// from how Rolldown treated it. See ADR-0002.
fn side_effects_pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("./").replace('\\', "/");
    fast_glob::glob_match(pattern.as_bytes(), path.as_bytes())
}
```

Keep `normalized_side_effect_path` — normalising *our* path to forward slashes before matching is our job, not the matcher's.

- [ ] **Step 6: Run everything that touches side effects**

```powershell
cargo test -p import-lens-daemon --test analyze --test candidate_matrix
```
Expected: PASS, **including matrix rows 42/43** (string and array globs on Windows). If a row that passed under the hand-rolled matcher now fails, that is the two engines disagreeing — investigate it, do not paper over it. `fast_glob` is the authority.

- [ ] **Step 7: Re-run the badge baseline — this is what it was built for**

```powershell
cargo test -p import-lens-daemon --release --locked --test candidate_badges -- --ignored --nocapture
```
Expected: PASS. Badges move in the "good" direction (more `truly_treeshakeable: true`, more High confidence) on real packages. **Any expectation in `EXPECTATIONS` that has to change is a finding, not a chore** — write down why.

- [ ] **Step 8: Update the SRS**

`docs/ImportLens-SRS.md`: the side-effects reporting contract now says Side-Effectful is a property of the import, and that array patterns are matched with Rolldown's own matcher.

- [ ] **Step 9: Commit**

```bash
git add daemon/src/pipeline/analyze.rs daemon/src/pipeline/resolver.rs daemon/Cargo.toml Cargo.lock scripts/compiler-stack.config.mjs scripts/compiler-stack.fingerprint.json daemon/tests/analyze.rs docs/ImportLens-SRS.md
git commit
```
Message: `fix(daemon)!: report array sideEffects against the entry, using Rolldown's matcher` — body must explain that every `["**/*.css"]` package was reported side-effectful and never tree-shakeable, that the justifying premise had already been retracted, and that the hand-rolled glob engine is replaced by the one Rolldown uses.

---

## Task 5: Unmeasured — no size without a build

**Files:**
- Modify: `daemon/src/pipeline/analyze.rs:150-165, 220-245, 245-265`
- Modify: `daemon/src/pipeline/fallback.rs` (delete two functions)
- Modify: `daemon/src/pipeline/mod.rs` (if it re-exports the deleted functions)
- Modify: `extension/src/ui/*` (render Unmeasured), `docs/logging-policy.md`
- Test: `daemon/tests/analyze.rs`

**Interfaces:**
- Produces: an `ImportResult` for an unbuildable import has **no size fields set** and carries `error` + `diagnostics`. Consumers must render "could not measure", never `0`.

**Why (ADR-0003):** three fallbacks fabricate a number where no build succeeded.

| Site             | Trigger                                              | What it reported                                                                                                                             |
| ---------------- | ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `analyze.rs:157` | manifest unreadable/unparseable                      | `approximate_directory_size` — the package **on disk**: unminified, uncompressed, tests, source maps, unused files. Overstates by up to 10×. |
| `analyze.rs:227` | entry over `MAX_MODULE_SOURCE_BYTES`                 | static sizing of **the entry file alone** — ignores the whole graph.                                                                         |
| `analyze.rs:260` | engine build failed (post-Task 1: also **panicked**) | same.                                                                                                                                        |

A large UI kit that breaches the graph limit is reported at the few kilobytes of its barrel when the true answer is megabytes. A confidence badge does not repair this — users read the byte count, and a number wrong by an order of magnitude while *looking* like a measurement is worse than no number, because it is actionable and the action is wrong. Task 1 makes the third row *more* reachable, so this cannot wait.

- [ ] **Step 1: Write the failing test**

```rust
/// ADR-0003. A build we could not perform yields no number -- not a plausible-looking one.
#[test]
fn a_failed_build_reports_no_size() {
    let fixture = write_package(
        r#"{ "name": "broken", "version": "1.0.0" }"#,
        "export const a = ((((;\n", // parse error
    );

    let result = analyze_named(&fixture, &["a"]).expect("analysis returns a result, not an Err");

    assert!(result.error.is_some(), "the failure must be reported");
    assert_eq!(result.raw_bytes, None, "no fabricated raw size");
    assert_eq!(result.minified_bytes, None, "no fabricated minified size");
    assert_eq!(result.brotli_bytes, None, "no fabricated compressed size");
    assert!(
        result.diagnostics.iter().any(|d| d.stage == "parse"),
        "the stage must say why"
    );
}

/// The manifest fallback measured the package's bytes ON DISK -- tests, source maps and
/// unused files included, unminified and uncompressed. That is not an Import Cost.
#[test]
fn an_unparseable_manifest_reports_no_size() {
    let fixture = write_package("{ not json", "export const a = 1;\n");

    let result = analyze_named(&fixture, &["a"]).expect("analysis returns a result");

    assert!(result.error.is_some());
    assert_eq!(result.brotli_bytes, None);
}
```

**DECIDED (owner, 2026-07-12): the size fields become `Option<u64>`.** They are bare `u64` today (`ipc/protocol.rs:189-193`). Sentinel zeroes were rejected: `0` is a *size*, and a consumer that forgets to check `error` renders **"0 B"** — telling the user an unbuildable import is **free**, which is precisely the class of wrong number ADR-0003 exists to abolish. `Option<u64>` makes that a compile error instead of a silent lie.

The ripple is real and is part of this task:

- `raw_bytes`, `minified_bytes`, `gzip_bytes`, `brotli_bytes`, `zstd_bytes` → `Option<u64>`.
- **The disk cache encodes these positionally** (`cache/disk.rs:1523` uses `rmp_serde::to_vec`, not `to_vec_named`), so the encoding changes. Bump `CURRENT_SCHEMA_VERSION` (`disk.rs:53`) **7 → 8**. The existing mechanism drops a mismatched DB, which is correct — old entries hold sizes for imports we would now call Unmeasured.
- Every consumer handles absence: `report/model.rs`, `extension/src/analysis/budgets.ts`, `insights.ts`, CodeLens, inlay hints, hover, the status bar, and the treemap (which must **exclude** Unmeasured rows, not treat them as `0`).

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p import-lens-daemon --test analyze reports_no_size`
Expected: FAIL — a size is present.

- [ ] **Step 3: Delete the three fallback arms**

In `daemon/src/pipeline/analyze.rs`:
- line ~157: replace `return approximate_manifest_fallback(context, request, error);` with `return Err(error);`
- line ~227: replace the oversized-entry `analyze_static_entry` block with an error result carrying `stage: "oversized_entry"` and the existing message.
- lines 258-263: replace the `Err(error) =>` fallback arm with `Err(error) => Err(error)`, so the whole `match` collapses to the `Ok` arm plus error propagation.

Then delete `analyze_static_entry` (its only two callers were 227 and 260) and, from `daemon/src/pipeline/fallback.rs`, delete `approximate_directory_size` and `estimate_minified_source`. `source_excerpt_detail` stays (used at `analyze.rs:301`). Per ADR-0002: **if OXC cannot minify it, we do not guess.**

`declaration_only_package_result` (types-only) is untouched — a types-only import genuinely costs zero bytes, which is a measurement, not a fabrication.

- [ ] **Step 4: Render it in the extension**

Every consumer of a size field must handle absence: CodeLens, inlay hints, hover, the status bar, the report rows, and the treemap (which must **exclude** Unmeasured rows, not treat them as `0`). The string is "could not measure", with the stage and diagnostics available on hover.

- [ ] **Step 5: Full diagnostics at warn (per owner direction)**

Update `docs/logging-policy.md`: an **Unmeasured** import logs its **full diagnostic vector at `warn`**, not `debug`. Rationale to record: after ADR-0003, "error and no measured size" is the entire failure path, and the diagnostics *are* the answer — requiring a log-level flip plus a reproduction is how one ends up debugging from partial evidence. Results that *did* produce a size keep the current debug-level treatment. The existing per-`(request_id, specifier, error)` dedup (FR-039c) bounds the noise.

Also record in `daemon/src/engine/adapter.rs`, at `classify_failure`, that **Rolldown's warnings are unrecoverable on a failed build in 1.1.5** (`HookBuildEndArgs` carries `errors` only; warnings reach us solely via `BundleOutput.warnings`, which does not exist when the build fails) — so a future reader does not mistake the gap for our own carelessness.

- [ ] **Step 6: Update the SRS**

`docs/ImportLens-SRS.md`: the failure contract. An unbuildable import has no size. Coverage drops; this is accepted.

- [ ] **Step 7: Run everything**

```powershell
cargo test -p import-lens-daemon --locked
pnpm check
pnpm test:ts
```
Expected: PASS. Several existing tests will assert fallback sizes — **each one must be re-read, not just re-baselined.** A test asserting "a failed build reports the entry file's size" was pinning the bug.

- [ ] **Step 8: Commit** (with Task 6 — see below)

---

## Task 6: An emitted asset is not a failure

**Files:**
- Modify: `daemon/src/engine/adapter.rs:259-280`
- Test: `daemon/tests/candidate_matrix.rs` (CSS row)

**Why:** `adapter.rs:266` fails any build where `chunks.len() != 1 || !asset_names.is_empty()`. The single-**chunk** half is a real invariant — it stops code-splitting from silently under-reporting. The **no-assets** half was never an invariant, only an assumption that nothing but JavaScript exists. The load hook deliberately lets Rolldown infer module type from the extension (`plugin.rs:276-279`), so a `.css` module becomes `ModuleType::Css` and Rolldown emits it as an asset — and **the build fails**.

Today this is masked: the failure degrades to the entry-file fallback and shows a plausible number. **Task 5 turns it into a blank row** for `swiper`, `react-datepicker`, `react-toastify` and every CSS-shipping UI kit. Task 5 creates this regression, so Task 6 ships with it. **They share a commit.**

- [ ] **Step 1: Write the failing row**

```rust
/// A package whose entry imports CSS must still be measurable. The adapter demanded
/// "one chunk AND no assets", and a `.css` module becomes an asset -- so every
/// CSS-shipping UI kit failed its build outright. Masked until ADR-0003 removed the
/// fallback that was quietly covering for it.
#[test]
fn matrix_47_a_css_importing_package_builds() {
    let fixture = write_node_modules_package_with_css(
        "withcss",
        r#"{ "name": "withcss", "version": "1.0.0" }"#,
        "import './styles.css';\nexport const button = () => 'ok';\n",
        ".btn { color: red; }\n",
    );

    let artifact = bundle_ok(BundleRequest {
        entries: vec![BundleEntry {
            entry_path: fixture.entry_path.clone(),
            package_root: fixture.package_root.clone(),
            selection: named(&["button"]),
            reported_side_effects: SideEffectsMode::Missing,
        }],
        runtime: ImportRuntime::Component,
        purpose: BundlePurpose::Import,
    });

    assert!(artifact.code.contains("button"), "the JS chunk is measured as before");
    assert!(
        artifact
            .diagnostics
            .iter()
            .any(|d| d.stage == "uncounted_assets"),
        "the uncounted non-JS bytes must be DISCLOSED, never silently omitted: {:?}",
        artifact.diagnostics
    );
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p import-lens-daemon --test candidate_matrix matrix_47`
Expected: FAIL — `expected exactly one chunk and no assets, got 1 chunk(s) and 1 asset(s) (styles.css)`.

- [ ] **Step 3: Relax the guard, disclose the gap**

In `daemon/src/engine/adapter.rs`, replace the `chunks.len() != 1 || !asset_names.is_empty()` failure with a single-chunk-only check, and push a diagnostic naming the uncounted bytes:

```rust
    // I19: an emitted asset is NOT a failure. The single-CHUNK guard stays -- it is what
    // stops code-splitting from silently under-reporting. "No assets" was never an
    // invariant, only an assumption that nothing but JavaScript exists: a `.css` module
    // becomes `ModuleType::Css` and Rolldown emits it as an asset, which failed the build
    // for every CSS-shipping package.
    if chunks.len() != 1 {
        return Err(/* existing output_shape failure, chunk count only */);
    }

    let asset_bytes: u64 = output
        .assets
        .iter()
        .filter_map(|item| match item {
            Output::Asset(asset) => Some(asset.source.as_bytes().len() as u64),
            Output::Chunk(_) => None,
        })
        .sum();

    if asset_bytes > 0 {
        // Counting these bytes into the Import Cost is the correct end state and is
        // deferred (see the release plan). Until then the gap is DISCLOSED, never
        // silently omitted -- a user comparing two datepickers must not be told the one
        // shipping heavy CSS is the cheaper one without warning.
        diagnostics.push(ImportDiagnostic {
            stage: "uncounted_assets".to_owned(),
            message: format!(
                "package also ships {asset_bytes} bytes of non-JavaScript assets \
                 ({}), which are not included in this size",
                asset_names.join(", ")
            ),
        });
    }
```

- [ ] **Step 4: Run it**

Run: `cargo test -p import-lens-daemon --test candidate_matrix matrix_47`
Expected: PASS.

- [ ] **Step 5: Verify the real CSS fixture is measurable end to end**

```powershell
cargo test -p import-lens-daemon --release --locked --test candidate_badges --test candidate_packages -- --ignored
```
`react-toastify` (added in Task 2) must produce a size and an `uncounted_assets` diagnostic — **not** an Unmeasured row.

- [ ] **Step 6: Update the SRS** — an import may carry uncounted non-JS bytes, and must disclose them.

- [ ] **Step 7: Commit Tasks 5 + 6 together**

```bash
git add daemon/src/pipeline/analyze.rs daemon/src/pipeline/fallback.rs daemon/src/engine/adapter.rs daemon/tests/ extension/src/ docs/logging-policy.md docs/ImportLens-SRS.md
git commit
```
Message: `fix!: report no size when a build fails, and stop failing on CSS` — body must explain that fabricated fallback sizes are gone (a graph-limit breach on a UI kit was reported at its barrel's few kilobytes), that CSS-shipping packages were failing their builds outright and would otherwise have gone blank, and that uncounted asset bytes are now disclosed.

---

## Task 7: Deterministic failure stage

**Files:**
- Modify: `daemon/src/engine/adapter.rs:286-366`
- Test: `daemon/tests/candidate_matrix.rs`

**Why:** `adapter.rs:297-301` picks the *first non-`link` diagnostic in Rolldown's vector*, and Rolldown accumulates diagnostics from module tasks running **concurrently** on the engine runtime. A build with a parse error in module A and an unresolved import in module B can report `stage: "parse"` on one run and `"resolve"` on the next, for identical inputs. The value is user-visible **and cached**, so whichever wins the race is frozen. §10.6 requires deterministic failure stages. After Task 5 the stage is the *primary* thing a user sees when there is no number.

Owner direction: *"it really doesn't matter what we show the user, either one is correct — the main thing is we need all errors in the logs, so that if we want to debug we don't get wrong info because of missing pieces."* All error diagnostics **are** already retained (`BundleFailure.message` joins every one; `.diagnostics` carries the full vector) — only the *label* was lossy. Task 5 Step 5 handles the logging half.

- [ ] **Step 1: Write the failing test**

```rust
/// Identical inputs must produce an identical stage. Rolldown accumulates diagnostics from
/// concurrent module tasks, so "first in the vector" is a race -- and the stage is cached.
#[test]
fn matrix_48_failure_stage_is_deterministic_across_runs() {
    let fixture = write_package_with_two_failures(); // parse error in A, unresolved import in B

    let stages: std::collections::BTreeSet<String> = (0..20)
        .map(|_| bundle_err(request_for(&fixture)).stage)
        .collect();

    assert_eq!(
        stages.len(),
        1,
        "the stage must not depend on which module task finished first: {stages:?}"
    );
    assert_eq!(
        stages.iter().next().map(String::as_str),
        Some("resolve"),
        "the earliest pipeline stage wins -- it names the cause, not the symptom"
    );
}
```

- [ ] **Step 2: Run it** — expect a flaky FAIL (may need several runs; that flakiness *is* the defect).

- [ ] **Step 3: Rank by pipeline order**

In `daemon/src/engine/adapter.rs`, replace the `.find(|stage| *stage != "link")` selection:

```rust
/// Earliest pipeline stage wins. Deterministic, needs no judgement to maintain, and names
/// CAUSES rather than symptoms -- a module that failed to resolve is often why something
/// downstream is malformed, and the later errors are its shrapnel.
fn stage_rank(stage: &str) -> u8 {
    match stage {
        "resolve" => 0,
        "load" => 1,
        "parse" => 2,
        "transform" => 3,
        "missing_export" => 4,
        "ambiguous_export" => 5,
        "link" => 6,
        "generate" => 7,
        _ => 8,
    }
}

    let stage = diagnostics
        .iter()
        .map(stage_for)
        .min_by_key(|stage| stage_rank(stage))
        .unwrap_or("link");
```

- [ ] **Step 4: Fix the warning stage (one line)**

`adapter.rs:358-366` stamps **every** warning with `stage: "generate"`, so an unresolved-import warning is labelled `generate`. Route warnings through `stage_for` like errors.

- [ ] **Step 5: Run** — `cargo test -p import-lens-daemon --test candidate_matrix` → PASS.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/engine/adapter.rs daemon/tests/candidate_matrix.rs
git commit
```
Message: `fix(daemon): rank failure stages deterministically` — body must explain the stage was decided by a race between concurrent module tasks and then cached, and that it is now the primary thing a user sees on an unmeasurable import.

---

## Task 8: The runtime partition

**Files:**
- Modify: `daemon/src/pipeline/file_size.rs:27-46, 200-225`
- Test: `daemon/tests/file_size.rs`

**Interfaces:**
- Consumes: Task 2's baseline (this moves file-size numbers).
- Produces: `FileSizeComputation` compressed totals are the **sum of per-runtime compressed bundles**; `shared_bytes` is partitioned by runtime.

**Why (ADR-0005):** two defects, one cause — nobody had said whether two runtimes in one file are one artifact or two. They are **two**.

1. **Compression.** `file_size.rs:210-222` joins the groups' minified output and compresses the **concatenation once**, so redundancy between the Client and Server bundles is compressed away — but they ship as two separately-compressed artifacts. The I15 amendment *accepted* this as a lower bound, reasoning that summing separately-compressed groups "would be a different lie (compression is not additive)". **That reasoning is wrong and I20 supersedes it:** non-additivity applies to parts that would in reality be compressed *together*. Two runtime groups never are. Summing their separately-compressed sizes models reality exactly; the concatenation is what distorts it. And after Task 9 this is the number the **per-file budget** gates on.
2. **Sharing.** `file_size.rs:27-46` counts a module as shared if it appears in more than one `ImportResult`, with **no runtime partition** — and `extension/src/analysis/insights.ts:112-137` renders that as a savings insight. A package imported from both Astro frontmatter (Server) and a client script (Client) is sold to the user as a shared dependency, claiming a deduplication the per-runtime build model **explicitly does not perform**. I15 already stated this must be partitioned; it never was.

- [ ] **Step 1: Write the failing tests**

```rust
/// Two runtimes are two artifacts. Compressing their concatenation once compresses away
/// redundancy between two payloads that never meet, under-reporting the file.
#[test]
fn mixed_runtime_compression_sums_the_groups_not_their_concatenation() {
    let document = document_importing_same_package_under_both_runtimes();

    let computed = compute_file_size(&document);
    let server_only = compute_file_size(&server_imports_of(&document));
    let client_only = compute_file_size(&client_imports_of(&document));

    assert_eq!(
        computed.brotli_bytes,
        server_only.brotli_bytes + client_only.brotli_bytes,
        "each runtime ships and compresses on its own"
    );
}

/// A module reached from two RUNTIMES is not shared -- each runtime ships its own copy.
/// Reporting it as shared sells the user a saving that does not exist.
#[test]
fn a_module_used_in_two_runtimes_is_not_shared() {
    let mut results = results_for_same_package_under_server_and_client();

    annotate_shared_bytes(&mut results);

    for result in &results {
        assert_eq!(
            result.shared_bytes,
            Some(0),
            "cross-runtime modules are not shared: each runtime ships its own copy"
        );
    }
}
```

**Note:** `annotate_shared_bytes` currently takes `&mut [ImportResult]` and `ImportResult` has no runtime field. It must take the runtime alongside each result — pass `&mut [(ImportRuntime, ImportResult)]`, or partition before calling. Choose the shape that keeps `file_size.rs`'s existing grouping (`BTreeMap<ImportRuntime, RuntimeGroup>`) as the single source of the partition; do not reintroduce a second one.

- [ ] **Step 2: Run and watch both fail.**

- [ ] **Step 3: Compress per group**

Replace the single-concatenation block (`file_size.rs:200-225`) with per-group compression, accumulating into `totals`. Delete the "lower bound" comment at lines 110-113 and the `minified_parts` join — `minified_bytes` becomes the sum of the groups' minified lengths (no join separator to account for).

- [ ] **Step 4: Partition sharing by runtime** — count module occurrences within a runtime group only.

- [ ] **Step 5: Run** — `cargo test -p import-lens-daemon --test file_size` → PASS.

- [ ] **Step 6: Fold the I15/I14 rationale into the design doc**

`file_size.rs:67` and `:161` cite "spec I15"/"I14" — a findings document **deleted in `76ca304`**. A reader has no authoritative record to check. Point both comments at the design doc's §6.3 amendments instead.

- [ ] **Step 7: Update the SRS** — mixed-runtime file sizes are the sum of per-runtime compressed bundles; cross-runtime modules are not shared.

- [ ] **Step 8: Commit**

```bash
git add daemon/src/pipeline/file_size.rs daemon/tests/file_size.rs docs/ImportLens-SRS.md
git commit
```
Message: `fix(daemon): treat each runtime as its own artifact` — body must explain that mixed-runtime files were under-reported (a lower bound presented as a size) and that cross-runtime modules were sold to users as a shared-dependency saving that never happens.

---

## Task 9: The aggregates

**Files:**
- Modify: `extension/src/analysis/budgets.ts:60-99`, `extension/src/listener.ts` (pass the file-size result)
- Modify: `daemon/src/report/model.rs:71,274,354`, `extension/src/ui/report.ts:137,73`
- Modify: `extension/src/analysis/insights.ts:177-193`
- Test: `extension/src/analysis/*.test.ts`, `daemon/tests/report.rs`

**Why (ADR-0004):** three surfaces treat per-import numbers as bundle quantities.

- **EXT-2 (worst, and free to fix).** `budgets.ts:67-99` sums each import's `brotli_bytes` into a file total — while `listener.ts:206-249` **already fetches** the deduplicated **File Cost** and `currentFileSize.ts:97` **already displays it**. A file with five `@mui/material` subpath imports sharing most of their graph is warned as 2–3× over budget while the status bar, one line away, shows it inside budget. The user is shown two totals for one file and the *wrong* one raises the diagnostic.
- **EXT-1.** `model.rs:71` sums per-import brotli and `report.ts:137` renders it as **"Total Brotli"**. The arithmetic is fine; the **word "Total" is the bug**. Deduplicating it would require the project-level union model ADR-0004 declines. So it is **relabelled Combined Import Cost**, and the report states that a dependency shared across files is counted at every site. Treemap percentages (`model.rs:354`) become a share of that. The duplicate-imports table (`model.rs:274`) becomes correct by label: `react` across fifty files genuinely *does* have a combined import cost of fifty Reacts, and that is the panel's point.
- **EXT-3.** `insights.ts:177-193` indexes shared modules by **specifier**; the daemon computes `shared_bytes` by **result**. `import React, { useState } from "react"` is *one specifier, two results* — so the daemon reports non-zero shared bytes while the extension finds no shared module to name and tells the user they are "outside the public top-module breakdown", which is **false**. Index by result.

- [ ] **Step 1: Write the failing budget test**

```ts
test("the file budget uses the deduplicated file cost, not a sum of imports", () => {
  // Five imports sharing most of their graph: summing them lands far above the single
  // bundle the daemon actually builds for the file (§6.3), which is what the status bar
  // already shows.
  const states = fiveSharedGraphImports(); // each 40 KB br
  const fileCost = { brotli_bytes: 55_000 };

  const violations = budgetViolationsForStates(states, { perFileBrotliBytes: 60_000 }, fileCost);

  assert.deepEqual(
    violations.filter((v) => v.kind === "file"),
    [],
    "the file is inside budget; summing the five would have raised a false violation",
  );
});
```

- [ ] **Step 2: Run and watch it fail** — the sum (200 KB) exceeds the 60 KB budget.

- [ ] **Step 3: Feed the budget the File Cost**

Change `budgetViolationsForStates(states, budgets)` to take the `FileSizeDocumentResponse` the controller already has, and delete the `fileBrotliBytes += actualBytes` accumulation. The **per-import** budget check is unchanged — it is genuinely per-import.

- [ ] **Step 4: Relabel the report**

`model.rs`: rename `total_brotli_bytes` → `combined_import_cost_brotli_bytes` (and the treemap denominator with it). `report.ts:137`: the metric is **"Combined Import Cost"**, with a one-line explanation that a dependency imported in several files is counted in each. Rename the duplicate-imports column likewise.

- [ ] **Step 5: Index shared modules by result**

`insights.ts:177-193`: key `sharedModuleIndex` by result identity, not `state.detected.specifier`, so the two results of `import React, { useState }` are two entries.

- [ ] **Step 6: Run** — `pnpm check && pnpm test:ts && cargo test -p import-lens-daemon --test report` → PASS.

- [ ] **Step 7: Update the SRS** — the workspace report's headline is a Combined Import Cost; the file budget gates on the File Cost.

- [ ] **Step 8: Commit**

```bash
git add extension/src daemon/src/report/model.rs docs/ImportLens-SRS.md
git commit
```
Message: `fix: stop reporting sums of import costs as totals` — body must explain the false budget warnings, the "Total Brotli" relabel and why deduplication was rejected (it is a bundle model this product does not have), and the shared-module tooltip lying about `import React, { useState }`.

---

## Task 10: Enumeration carries its runtime

**Files:**
- Modify: `daemon/src/ipc/service.rs:1614`, `daemon/src/ipc/protocol.rs:686-698`
- Modify: `extension/src/ipc/protocol.ts`, the enumeration caller
- Test: `daemon/tests/enumerate.rs`

**Why:** `service.rs:1614` hardcodes `runtime: ImportRuntime::Component`. Component/Client resolve with `alias_fields = ["browser"]` and browser conditions; Server resolves with node conditions (`resolver.rs:249,578`). So in a Server-context file (Astro frontmatter is Server for sizing), a package whose `exports` map diverges across `node` and `browser` is enumerated under **browser** conditions while the *size* of that same import is correctly computed under **Server**. The completion list omits names the file can import and offers names it cannot. Completions and sizing disagree by construction.

**SF-11 is half wrong** and the review says so no longer: `CompleteImportMembersRequest` (`protocol.rs:611-621`) **already carries `source` and `cursor_offset`**, and `document/script_regions.rs:123-150` **already classifies a document offset into a runtime**. So that path needs **no protocol change** — run the existing classifier. Only `EnumerateExportsRequest` lacks the input.

Per ADR-0002: **one classifier, not two.** The daemon derives the runtime; the extension does not compute and send it, or the bug reappears as a disagreement between two implementations.

- [ ] **Step 1: Write the failing test** — a package with divergent `node`/`browser` export maps, enumerated from an Astro frontmatter offset, must return the **node** surface.

- [ ] **Step 2: Run and watch it fail** (browser names returned).

- [ ] **Step 3: Derive the runtime in `service.rs`** — for `CompleteImportMembers`, classify `cursor_offset` against `source` via `script_regions`. For `EnumerateExports`, add an **optional offset field** to the request and classify it; absent (a plain `.ts` file), the classifier's default is `Component`, which is correct.

- [ ] **Step 4: Run** → PASS.

- [ ] **Step 5: Note the memo dimension coming alive** — `export_list.rs`'s memo is already keyed by runtime, but production only ever wrote the `Component` key. That dimension is now live, so **enumerations cached before this change must not be trusted after it** (Task 14's `ANALYZER_REVISION` bump covers this).

- [ ] **Step 6: Commit** (with Task 11 — same file, same subsystem)

---

## Task 11: The enumeration memo must expire

**Files:**
- Modify: `daemon/src/pipeline/export_list.rs:37-45`
- Test: `daemon/tests/enumerate.rs`

**Why:** the memo stores only `enumeration.read_time_fingerprints` — **source modules**. The size path deliberately adds the root and first-party manifests (`analyze.rs:410-412`); enumeration does not. And the memo has **no TTL**. So for a first-party/workspace package under development, flipping `"type": "module"` (or editing `exports`/`sideEffects`) in its `package.json` moves no source file, changes no fingerprint, and bumps no cache generation — and the completion popup serves the **old export list indefinitely**. §8.3 already requires manifests to be freshness inputs.

- [ ] **Step 1: Write the failing test** — enumerate a first-party package, edit only its `package.json`, enumerate again, assert the list refreshes.

- [ ] **Step 2: Run and watch it fail** (stale list served).

- [ ] **Step 3: Add the manifests to the memo's fingerprints** — reuse `first_party_manifests` / `full_package_fingerprints` from `analyze.rs` rather than writing a second manifest walker.

- [ ] **Step 4: Run** → PASS.

- [ ] **Step 5: Commit Tasks 10 + 11 together**

```bash
git add daemon/src/ipc/service.rs daemon/src/ipc/protocol.rs daemon/src/pipeline/export_list.rs extension/src/ipc/protocol.ts daemon/tests/enumerate.rs
git commit
```
Message: `fix(daemon): enumerate exports under the import's runtime, and expire the memo on manifests` — body must explain that Server-context completions were resolved under browser conditions (so completions and sizing disagreed), and that a first-party `package.json` edit served a stale export list forever.

---

## Task 12: `deps:update:safe` must restore what it recorded

**Files:**
- Modify: `scripts/deps-update-safe.mjs:41-48`
- Test: `scripts/deps-update-safe.test.mjs`

**Why:** the restore loop builds its pins from the **direct crates only** — 11 packages (`rolldownFamilyCrates()` + `oxc_resolver` + `oxcCrates`) — while the recorded set in `scripts/compiler-stack.fingerprint.json` is **52** (53 after Task 4). The script's own comment concedes that Rolldown's caret ranges let a general update move its workspace crates; the restore loop then never touches them. The rolldown 1.1.5 registry manifest confirms every workspace sibling is a **caret** range, so this is not merely possible but **inevitable on the next upstream patch**: `cargo update` moves `rolldown_utils`/`rolldown_plugin_*` within their carets, the restore fixes 11, the fingerprint still mismatches, and the command **fails for exactly the case §4.4 says it should have restored** — leaving a mutated `Cargo.lock` and `pnpm-lock.yaml` with no recovery but `git checkout`.

Task 4 adds `fast-glob` to the fingerprint, so this stops being theoretical.

- [ ] **Step 1: Write the failing test**

A **Drift** test: derive the expected restore-pin set from the fingerprint file and assert the script pins all of them.

```js
test("the restore loop pins every package the fingerprint records", async () => {
  const fingerprint = JSON.parse(await readFile(FINGERPRINT_PATH, "utf8"));
  const pinned = await restorePinsForTest(); // export the pin list from the script

  const missing = fingerprint.packages
    .map((pkg) => pkg.name)
    .filter((name) => !pinned.some(([crate]) => crate === name));

  assert.deepEqual(
    missing,
    [],
    "deps:update:safe cannot restore a package it never pins; these would leave the " +
      "fingerprint mismatched with no recovery but `git checkout`",
  );
});
```

- [ ] **Step 2: Run and watch it fail** — ~41 packages missing.

- [ ] **Step 3: Derive the pins from the fingerprint**

```js
  // §4.4: restore EVERY compiler-stack package to the recorded version, not just the 11
  // we depend on directly. Rolldown's workspace siblings are caret ranges, so a general
  // `cargo update` moves them within their carets -- and pinning only the direct crates
  // left the fingerprint mismatched and the lockfiles mutated with no way back.
  const fingerprint = JSON.parse(
    await readFile(path.join(rootDir, FINGERPRINT_PATH), "utf8"),
  );
  const pins = fingerprint.packages.map((pkg) => [pkg.name, pkg.version]);
```

- [ ] **Step 4: Run** → PASS. Then exercise it for real: `pnpm deps:update:safe` on a dirty lockfile must restore and exit 0.

- [ ] **Step 5: Commit**

```bash
git add scripts/deps-update-safe.mjs scripts/deps-update-safe.test.mjs
git commit
```
Message: `fix(scripts): restore the whole compiler stack, not just its direct crates` — body must explain that a caret-range move in Rolldown's workspace siblings made the command fail for the exact case it exists to handle.

---

## Task 13: Raise `ENGINE_PERMITS`

**Files:**
- Modify: `daemon/src/engine/boundary.rs:22`

**Why (§6 improvement 1, §10.7 explicitly authorizes one bounded tuning pass on the build-concurrency limit):** the 20-import batch measures **78 MB peak against a 400 MB gate** — 5× headroom — while 20 misses serialize into 10 sequential rounds at 2 permits. Permits bound **peak memory**, not speed, and there is memory to spare.

**Sequenced after Task 2 deliberately:** the §10.6 memory gate has never run. Raising the permit count without it would be asserting an improvement rather than measuring one.

- [ ] **Step 1: Raise it**

```rust
/// Permits bound peak MEMORY, not speed. The 20-import batch peaks at 78 MB against the
/// §10.6 400 MB gate -- 5x headroom -- while 20 misses serialized into 10 rounds at two
/// permits. §10.7 authorizes one bounded tuning pass on this limit.
pub fn engine_permits() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().clamp(2, 4))
        .unwrap_or(2)
}
```

`ENGINE_PERMITS` is a `const` used by `Semaphore::const_new` and by the drain worker sizing (`scheduling.rs`). Converting it to a function means the semaphore becomes a `LazyLock<Semaphore>`. Update both call sites.

- [ ] **Step 2: Measure, do not assert**

```powershell
cargo test -p import-lens-daemon --release --locked --test candidate_performance -- --ignored --nocapture
```
Record cold p95, 20-import batch wall time, and **peak RSS** before and after in the commit body. If peak RSS approaches the 400 MB gate, **lower the clamp** — the gate is the authority, not the expectation.

- [ ] **Step 3: Commit**

```bash
git add daemon/src/engine/boundary.rs daemon/src/pipeline/scheduling.rs
git commit
```
Message: `perf(daemon): admit up to four concurrent builds` — body must carry the measured before/after numbers, including peak RSS against the 400 MB gate.

---

## Task 14: Bump the revision, package, release

**Files:**
- Modify: `daemon/src/cache/key.rs:43-49` (`ANALYZER_REVISION`)
- Modify: `extension/src/daemon/knownHashes.generated.ts` (generated)
- Modify: `docs/ImportLens-SRS.md`, `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` (status header)

- [ ] **Step 1: Bump `ANALYZER_REVISION`** `rolldown2` → `rolldown3`. **Required:** Tasks 3, 4, 5, 6, 8 and 10 all moved reported numbers or cached claims, and cached entries record the revision they were computed under. Enumerations cached before Task 10 are keyed under a `Component` runtime that no longer means what it did.

- [ ] **Step 2: Full verification**

```powershell
pnpm check
pnpm test
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: all green.

- [ ] **Step 3: Real-package and accuracy gates**

```powershell
node scripts/prepare-candidate-fixtures.mjs "$env:TEMP/candidate-fixtures"
$env:IMPORT_LENS_FIXTURES_WORKSPACE="<path>"
cargo test -p import-lens-daemon --release --locked --test candidate_packages --test candidate_badges --test candidate_performance -- --ignored --nocapture
$env:IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES="1"
node scripts/accuracy-compare.mjs
```
Expected: all green. Record the accuracy deltas — they **will** have moved (Tasks 3 and 4 change what is retained). If any package now exceeds the 25% tolerance, that is a finding, not a baseline to widen.

- [ ] **Step 4: Package**

```powershell
pnpm package:win32-x64
```
This rebuilds the daemon, copies the Windows binary, refreshes `extension/src/daemon/knownHashes.generated.ts`, builds the extension bundle, and creates the VSIX. Confirm the VSIX size is inside its gate — the unwind profile already cost 4.44 → 5.42 MB.

- [ ] **Step 5: Re-baseline §10.7 with MEASURED numbers** — not asserted ones. Update the design doc's status header: the release amendments I16–I23 are implemented.

- [ ] **Step 6: Commit**

```bash
git add daemon/src/cache/key.rs extension/src/daemon/knownHashes.generated.ts docs/
git commit
```
Message: `chore(release): bump the analyzer revision and refresh the daemon hash` — body must list which changes moved numbers and why the cache must be invalidated.

---

## Deferred, deliberately (not oversights)

- **Count non-JavaScript bytes.** Fold CSS/wasm/font bytes into the Import Cost, with per-artifact compression (ADR-0005), a CSS matrix row asserting the bytes, and the esbuild oracle configured to emit CSS. Touches the engine contract (`BundleArtifact` must carry assets), both pipelines, the module breakdown and the accuracy harness, and moves numbers on a whole category of packages. **Task 6 discloses the gap in the meantime** — it is never silently omitted.
- **An honest lower bound on failed builds** ("at least 4 MB; graph limit exceeded") — the intended successor to ADR-0003. A limit breach means much of the graph *was* loaded before we stopped, so a real floor exists. The engine discards the partial graph on failure, so this needs plumbing through the engine boundary; it does not belong inside a stability fix.
- **§6 improvements 2–22** (prewarm priority inversion, the per-module source clone, the chunk clone, the LRU dependency-path index, the thread-spawning drain, `BundlePurpose`'s zero readers, the IPC runtime width). All real; all performance and hygiene on a path that already meets its gates. Shipping them beside this many semantic changes means an unexplained number movement has too many candidate causes.
- **Marginal Cost / a project-level bundle model** — ADR-0004. A different product, decided on its own merits.
