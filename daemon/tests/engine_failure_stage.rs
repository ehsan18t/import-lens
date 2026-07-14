//! The failure stage is a DURABLE, user-visible value, so it may not be decided by a race (§10.6).
//!
//! Under [ADR-0006](../../docs/adr/0006-the-result-model.md) an import whose build failed reports
//! **no size at all** — so the stage is not a footnote beside a number, it is the entire answer.
//! And a deterministic stage is *cached*: whichever value wins is frozen into the durable answer
//! for as long as the package's bytes are unchanged.
//!
//! Rolldown fans its module tasks out onto the async runtime (`tokio::spawn` per module) and
//! accumulates their diagnostics in the order the tasks report — `consolidate_diagnostics` merges
//! only tsconfig errors and otherwise preserves that arrival order. So a build whose modules fail
//! at DIFFERENT stages hands the adapter a vector whose order is a race, and byte-identical inputs
//! can produce two different answers on two runs of the same daemon.
//!
//! Every test here therefore runs its build MANY times. A single-error fixture would prove
//! nothing: the race needs at least two module tasks failing at different stages.

use import_lens_daemon::engine::{
    BundleEntry, BundleFailure, BundlePurpose, BundleRequest, BundleSelection, ImportRuntime,
    RolldownEngine, boundary, stage,
};
use import_lens_daemon::pipeline::stage::may_enter_a_durable_store;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

mod common;

/// Enough runs to expose a task-completion race, cheaply: each build is four tiny modules and
/// fails before linking.
const RUNS: usize = 48;

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn request(root: &Path, entry: &str, names: &[&str]) -> BundleRequest {
    BundleRequest {
        entries: vec![BundleEntry {
            entry_path: root.join(entry),
            package_root: root.to_path_buf(),
            selection: BundleSelection::Named(
                names.iter().map(|name| (*name).to_owned()).collect(),
            ),
        }],
        runtime: ImportRuntime::default(),
        purpose: BundlePurpose::ImportSize,
    }
}

/// A diagnostic as it reaches the pipeline: stage plus message, both durable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Diagnostic {
    stage: String,
    message: String,
}

/// Everything about a failure that reaches the user or the cache.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Failure {
    stage: String,
    message: String,
    diagnostics: Vec<Diagnostic>,
}

impl From<BundleFailure> for Failure {
    fn from(failure: BundleFailure) -> Self {
        Self {
            stage: failure.stage,
            message: failure.message,
            diagnostics: failure.diagnostics.iter().map(Diagnostic::from).collect(),
        }
    }
}

impl From<&import_lens_daemon::engine::ImportDiagnostic> for Diagnostic {
    fn from(diagnostic: &import_lens_daemon::engine::ImportDiagnostic) -> Self {
        Self {
            stage: diagnostic.stage.clone(),
            message: diagnostic.message.clone(),
        }
    }
}

/// One build of `entry`, driven through the engine on its own runtime — the same way
/// `boundary::bundle_sync` drives it, minus the permit pool this file has no use for.
fn bundle(root: &Path, entry: &str, names: &[&str]) -> Result<Vec<Diagnostic>, Failure> {
    let request = request(root, entry, names);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test engine runtime should build");

    match runtime.block_on(RolldownEngine.bundle(request)) {
        Ok(artifact) => Ok(artifact.diagnostics.iter().map(Diagnostic::from).collect()),
        Err(failure) => Err(Failure::from(failure)),
    }
}

/// The number of valid statements in front of `broken.js`'s syntax error.
///
/// **It is what makes the race observable, and it is not a trick.** With a one-line `broken.js`,
/// its task always reported before the resolve failure could and the old adapter answered `parse`
/// 48 times out of 48 — a stable-looking wrong answer. Padding the module makes its parse take
/// about as long as the other task's failed resolve, and the pre-fix adapter then answered `parse`
/// 42 times and `resolve` 6 times **for the same bytes**. The size of an unrelated module is not
/// something a durable answer may depend on; that it can flip the label is the whole defect.
const PARSE_TASK_PADDING: usize = 4_000;

/// A package whose entry pulls in three modules that fail in three different module TASKS:
///
/// * `unresolved.js` — a relative import of a file that is not there. A path-like specifier that
///   does not resolve is a build **error** in Rolldown 1.1.5 (only a *bare* one is externalized
///   with a warning), so it fails the build at `resolve`.
/// * `broken.js` — a syntax error behind [`PARSE_TASK_PADDING`] valid statements: `parse`.
/// * `also-broken.js` — a second `parse` error, so the vector is genuinely contended rather than
///   two tasks racing in lockstep.
///
/// All three are dependencies of one entry, so Rolldown spawns their tasks together and the order
/// their diagnostics land in is decided by whichever finishes first.
fn write_multi_stage_failure(root: &Path) {
    write_source(
        root,
        "entry.js",
        "export { late } from './unresolved.js';\nexport { broken } from './broken.js';\n\
         export { alsoBroken } from './also-broken.js';",
    );
    write_source(
        root,
        "unresolved.js",
        "export { late } from './nowhere.js';",
    );
    let padding = (0..PARSE_TASK_PADDING).fold(String::new(), |mut source, index| {
        source.push_str(&format!("export const pad_{index} = {index};\n"));
        source
    });
    write_source(
        root,
        "broken.js",
        &format!("{padding}export const broken = ;"),
    );
    write_source(root, "also-broken.js", "export const alsoBroken = ;;;=");
}

const SELECTION: &[&str] = &["late", "broken", "alsoBroken"];

/// **The defect.** The reported stage must be the same on every run of a byte-identical build.
///
/// It is ranked by PIPELINE POSITION and the earliest wins — `resolve` here — because the earliest
/// failure is the likeliest root cause and the later ones are frequently its shrapnel. The old code
/// took "the first diagnostic that is not `link`": whichever module task happened to report first.
#[test]
fn the_reported_stage_of_a_multi_stage_failure_is_the_same_on_every_run() {
    let root = common::temp_workspace("import-lens-failure-stage");
    write_multi_stage_failure(&root);

    let mut stages = BTreeSet::new();
    for _ in 0..RUNS {
        let failure = bundle(&root, "entry.js", SELECTION)
            .expect_err("a package with a parse error and an unresolved import cannot build");
        stages.insert(failure.stage);
    }

    assert_eq!(
        stages,
        BTreeSet::from([stage::RESOLVE.to_owned()]),
        "a byte-identical build reported more than one stage across {RUNS} runs: the stage is \
         decided by whichever of Rolldown's concurrently-spawned module tasks reported first, and \
         it is both what the user sees and what the cache stores (ADR-0006, §10.6)"
    );

    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

/// The stage is not the only durable value that vector decides. The message and the diagnostic list
/// are rendered FROM it, so they must be ordered too — otherwise the cached answer for unchanged
/// bytes still differs run to run, just further down the response.
#[test]
fn the_message_and_the_diagnostic_list_of_a_multi_stage_failure_are_the_same_on_every_run() {
    let root = common::temp_workspace("import-lens-failure-message");
    write_multi_stage_failure(&root);

    let mut failures = BTreeSet::new();
    for _ in 0..RUNS {
        let failure = bundle(&root, "entry.js", SELECTION)
            .expect_err("a package with a parse error and an unresolved import cannot build");
        failures.insert(failure);
    }

    assert_eq!(
        failures.len(),
        1,
        "a byte-identical build produced {} different failures across {RUNS} runs: {failures:#?}",
        failures.len()
    );

    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

/// The stage on the SUCCESS path, which is where most users will meet one.
///
/// An unresolved BARE import is a warning, not an error: Rolldown externalizes it and the build
/// succeeds (construct matrix rows 24/25). The adapter stamped every warning `generate` — so the
/// one diagnostic that says "this package imports something that is not installed, and its bytes
/// are NOT in this number" was labelled a code-generation problem. Warnings go through the same
/// `stage_for` mapping the errors do.
#[test]
fn a_warning_carries_the_stage_it_actually_came_from() {
    let root = common::temp_workspace("import-lens-warning-stage");
    write_source(
        &root,
        "entry.js",
        "export { thing } from 'unresolvable-pkg';",
    );

    let diagnostics = bundle(&root, "entry.js", &["thing"])
        .expect("an unresolvable BARE import is externalized, and the build succeeds");
    let unresolved = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("unresolvable-pkg"))
        .unwrap_or_else(|| panic!("the unresolved import must be disclosed: {diagnostics:#?}"));

    assert_eq!(
        unresolved.stage,
        stage::RESOLVE,
        "an unresolved import is a resolve failure, whichever side of the build it lands on"
    );

    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

/// The same rule, one layer down: the graph-limit breach.
///
/// A `module_graph_limit` failure does not come from a Rolldown diagnostic — the plugin records it
/// and the adapter reports it — so it is not ordered by the ranking above. Its message named
/// whichever module breached FIRST, and the plugin's hooks run on the same concurrently-spawned
/// module tasks: a graph with two oversized modules named `a.js` on some runs and `b.js` on others,
/// **2 distinct messages over 24 runs of the same bytes** before the fix. The stage was never in
/// doubt; the message was, and a `module_graph_limit` failure is deterministic, so that message is
/// cached and shown.
///
/// The oversized modules cost nothing to build with: the `load` hook refuses a module over the
/// limit on its `metadata` alone, without ever reading it.
#[test]
fn the_breach_message_of_a_multi_breach_graph_is_the_same_on_every_run() {
    let root = common::temp_workspace("import-lens-breach-message");
    let oversized = "A".repeat(21 * 1024 * 1024);
    write_source(
        &root,
        "entry.js",
        "export { a } from './a.js';\nexport { b } from './b.js';",
    );
    write_source(&root, "a.js", &format!("export const a = \"{oversized}\";"));
    write_source(&root, "b.js", &format!("export const b = \"{oversized}\";"));

    let mut failures = BTreeSet::new();
    for _ in 0..RUNS {
        let failure = bundle(&root, "entry.js", &["a", "b"])
            .expect_err("two oversized modules cannot be built");
        assert_eq!(failure.stage, stage::MODULE_GRAPH_LIMIT, "{failure:?}");
        failures.insert(failure.message);
    }

    assert_eq!(
        failures.len(),
        1,
        "a byte-identical build named {} different breaching modules across {RUNS} runs — the \
         message is decided by whichever module task the runtime finished first, and it is cached: \
         {failures:#?}",
        failures.len()
    );

    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

/// **Property** over the whole engine vocabulary: no deterministic stage can outrank a transient
/// one.
///
/// This is the invariant that must be impossible to get wrong, because getting it wrong caches a
/// transient failure — the defect that recurred seven times on this branch (ADR-0006). A ranking
/// that let `parse` beat `timeout` would report a build that never finished as a fact about the
/// package's bytes, and the durable-store allowlist would then wave it through, because `parse` is
/// on that allowlist.
///
/// It quantifies over `stage::ALL`, so a stage added tomorrow is ranked against every transient
/// stage without anyone remembering to come back here.
#[test]
fn no_deterministic_stage_can_outrank_a_transient_one() {
    let (transient, deterministic): (Vec<&str>, Vec<&str>) = stage::ALL
        .iter()
        .copied()
        .partition(|candidate| stage::is_transient(candidate));

    assert!(!transient.is_empty() && !deterministic.is_empty());

    for moment in &transient {
        assert!(
            !may_enter_a_durable_store(moment),
            "`{moment}` is transient, so no store may take it"
        );
        for fact in &deterministic {
            assert!(
                stage::rank(moment) < stage::rank(fact),
                "`{moment}` describes this run of the daemon and `{fact}` describes the package's \
                 bytes. Ranking `{fact}` ahead of `{moment}` would let a build that panicked, timed \
                 out, or lost its runtime be reported as a deterministic failure — and a \
                 deterministic failure is CACHED (ADR-0006, invariant 3)"
            );
        }
    }
}

/// Every declared stage has its own place in the order. A stage that exists but is not ranked would
/// fall into the unknown-stage bucket and sort last by accident rather than by decision — exactly
/// the drift the single-source vocabulary exists to prevent.
#[test]
fn every_declared_stage_has_a_distinct_rank() {
    let ranks: BTreeSet<usize> = stage::ALL.iter().map(|name| stage::rank(name)).collect();

    assert_eq!(
        ranks.len(),
        stage::ALL.len(),
        "two stages share a rank, or one is missing from the order: {:?}",
        stage::ALL
    );
    assert!(
        ranks.iter().all(|rank| *rank < stage::ALL.len()),
        "no declared stage may land in the unknown-stage bucket"
    );
}

/// The other half of the transient answer, and the reason the ranking above is a belt rather than
/// the braces: **a transient outcome never reaches the ranking at all.**
///
/// A `panic` / `timeout` / `engine_gone` is not a Rolldown diagnostic — it is constructed in
/// `boundary.rs` at a point where the build's diagnostics do not exist (a panic unwinds straight
/// past `classify_failure`; a timeout drops the future; a dead runtime never replies). So a
/// BundleFailure is EITHER transient with nothing to rank, OR deterministic with its diagnostics.
/// The two cannot be mixed inside one failure, and this is what says so.
///
/// (Where a transient stage and a deterministic result genuinely DO coexist — a measured import
/// whose full-package comparison build timed out — the stage is not chosen by a ranking either: the
/// failure rides along as a *diagnostic* on a Measured result, and `ImportResult::is_durable`
/// refuses the whole result if ANY diagnostic names a non-durable stage. `service.rs` pins that.)
#[test]
fn a_lost_build_reports_a_transient_stage_and_brings_no_diagnostics_to_rank() {
    let failure = boundary::bundle_sync_for_test_panic()
        .expect_err("the synthetic build panics inside the boundary");

    assert!(
        stage::is_transient(&failure.stage),
        "a build that unwound describes the moment, not the package: {failure:?}"
    );
    assert!(
        failure.diagnostics.is_empty(),
        "a transient failure carries no diagnostics, so nothing can outrank its stage: {failure:?}"
    );
    assert!(
        !may_enter_a_durable_store(&failure.stage),
        "`{}` must never enter a durable store",
        failure.stage
    );
}
