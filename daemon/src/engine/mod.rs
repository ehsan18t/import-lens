//! The Rolldown bundling engine (bundler redesign spec §5). This surface is
//! Import Lens-owned: no Rolldown type may appear in any public field,
//! argument, or return type, and only `adapter.rs`/`plugin.rs` may import
//! the crate.

mod adapter;
pub mod boundary;
pub(crate) mod dependency_paths;
mod entry;
pub(crate) mod limits;
mod plugin;
pub(crate) mod scheduling;

use std::path::PathBuf;

pub use crate::ipc::protocol::ImportRuntime;
pub use adapter::RolldownEngine;

#[derive(Debug, Clone)]
pub struct BundleRequest {
    pub entries: Vec<BundleEntry>,
    pub runtime: ImportRuntime,
    pub purpose: BundlePurpose,
}

/// An entry to measure. It carries **no `sideEffects` metadata**, and that is the contract, not an
/// omission: Rolldown reads the package's `sideEffects` itself, from the manifest the plugin
/// supplies alongside the entry, and it is the only authority on retention (FR-021). The daemon's
/// own reading of the field is *reporting* metadata — it decides a badge, never a byte — so it
/// belongs on the pipeline's side of this boundary and stays there. The field used to be here, and
/// its one and only reader was a diagnostic justified by a premise that has since been retracted.
#[derive(Debug, Clone)]
pub struct BundleEntry {
    /// Pre-resolved absolute entry file; the engine never re-resolves the
    /// bare package specifier.
    pub entry_path: PathBuf,
    pub package_root: PathBuf,
    pub selection: BundleSelection,
}

#[derive(Debug, Clone)]
pub enum BundleSelection {
    Named(Vec<String>),
    Default,
    Namespace,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundlePurpose {
    ImportSize,
    FileSize,
    FullPackageComparison,
    ExportEnumeration,
}

#[derive(Debug, Clone)]
pub struct ModuleContribution {
    pub path: PathBuf,
    pub rendered_bytes: usize,
}

/// A non-JavaScript module the graph imported: real bytes that ship with the package and are NOT
/// in the measured size, because the measured size is the JavaScript chunk.
///
/// Almost always a stylesheet. Rolldown 1.1.5 cannot bundle CSS at all, so the plugin links it as
/// an empty module and records it here; disclosing it is the honest alternative to counting bytes
/// the bundler never rendered, or to failing the build and reporting nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncountedAsset {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ImportDiagnostic {
    pub stage: String,
    pub message: String,
}

#[derive(Debug)]
pub struct BundleArtifact {
    /// Unminified source of the single output chunk.
    pub code: String,
    pub loaded_paths: Vec<PathBuf>,
    /// Fingerprints captured when each module's bytes were read during this build,
    /// so freshness describes the bytes the size was actually measured from (§8.3).
    pub read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    /// Loaded paths with no read-time fingerprint — binary modules the plugin handed
    /// back to Rolldown. The caller fingerprints these by reading them.
    pub unhashed_paths: Vec<PathBuf>,
    pub contributions: Vec<ModuleContribution>,
    /// The chunk's public export list. Deliberately kept despite having no production
    /// reader: it is how the qualification suites assert that every requested
    /// `__il_entry_*` alias survived linking, which is the invariant the whole
    /// selection mechanism rests on (§8.4). Removing it would cost one small
    /// allocation per build and take that check with it.
    pub exported_names: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub matched_side_effect_paths: Vec<PathBuf>,
    /// Bytes this size does NOT include (see [`UncountedAsset`]). Already summarized into a
    /// `uncounted_assets` diagnostic; kept structured so a future surface can show them.
    pub uncounted_assets: Vec<UncountedAsset>,
}

/// The result of export enumeration (§8.4).
///
/// Carries diagnostics on the *success* path: Rolldown reports a missing or ambiguous
/// export as an error, which already reaches the user, but a build that succeeds with
/// warnings had those warnings silently dropped when this was a bare `Vec<String>`.
///
/// It also carries the build's read-time fingerprints, which is what lets the caller
/// memoize the enumeration instead of running a full engine build of the whole package
/// graph on every completion popup.
#[derive(Debug, Clone)]
pub struct ExportEnumeration {
    pub names: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    /// Loaded paths with no read-time fingerprint. A non-empty list means the
    /// enumeration must not be memoized: there is nothing to expire it against.
    pub unhashed_paths: Vec<PathBuf>,
}

/// The closed vocabulary of `BundleFailure::stage` (§12).
///
/// This module is the single source of truth. `pipeline::analyze::contract_stage` derives
/// its mapping from [`stage::ALL`] rather than restating the list, so a stage cannot exist
/// here and be silently relabelled `generate` on the way to the client. Every
/// `BundleFailure` the daemon constructs takes its stage from one of these constants; a new
/// stage that is added as a bare string literal at a construction site instead is exactly
/// the drift this module exists to make impossible, and `daemon/src/engine` is guarded
/// against it.
pub mod stage {
    /// Declares the vocabulary. Each constant and its membership in [`ALL`] are emitted from
    /// the *same* line of the same invocation, so a stage that exists but is missing from
    /// `ALL` — and is therefore relabelled `generate` at the contract edge while
    /// `file_size.rs` passes it through untouched, one failure under two names — is not a
    /// mistake you can make here. It is unrepresentable rather than merely tested for.
    macro_rules! stages {
        ($($(#[$attribute:meta])* $name:ident => $value:literal,)+) => {
            $($(#[$attribute])* pub const $name: &str = $value;)+

            /// Every stage declared above, in contract order. Anything absent from this list
            /// would collapse to [`GENERATE`] at the contract edge — which is why the list is
            /// generated from the declarations instead of restated beside them.
            pub const ALL: &[&str] = &[$($name),+];
        };
    }

    stages! {
        RESOLVE => "resolve",
        PARSE => "parse",
        LINK => "link",
        GENERATE => "generate",
        OUTPUT_SHAPE => "output_shape",
        MODULE_GRAPH_LIMIT => "module_graph_limit",
        MISSING_EXPORT => "missing_export",
        AMBIGUOUS_EXPORT => "ambiguous_export",
        /// A build that unwound into the boundary's `catch_unwind`.
        PANIC => "panic",
        /// A build that did not finish within `boundary::BUILD_TIMEOUT` and was cancelled.
        /// That is the whole of its meaning: it says nothing about how long a *request* took,
        /// because a request no longer waits for every build it triggers (§9).
        TIMEOUT => "timeout",
        /// The engine runtime dropped the build without replying.
        ENGINE_GONE => "engine_gone",
    }

    /// Whether a stage describes a failure of **this run of the daemon** rather than of the
    /// package.
    ///
    /// A `parse`/`link`/`resolve`/`output_shape`/`module_graph_limit` failure is a property of
    /// the code being measured: it will fail the same way next time, so the degraded result it
    /// produces is worth caching. These three are not. A build that was cancelled at the
    /// deadline, unwound, or lost its runtime tells us nothing about the package, so storing what
    /// it produced makes a scheduling accident durable — and durable is forever, next to a build
    /// that would have succeeded on the retry nobody will now run.
    ///
    /// This is the ENGINE's list, and it is not by itself the cache gate: a stage can be transient
    /// in fact without being an engine stage at all (`pipeline::stage::ENTRY_METADATA` is
    /// `fs::metadata` failing). The gate is the allowlist in
    /// `crate::pipeline::stage::may_enter_a_durable_store`, which every store applies through
    /// [`crate::ipc::protocol::ImportResult::is_durable`]; the L1 aggregate additionally refuses a
    /// total that summed an import nobody had measured
    /// ([`crate::pipeline::file_size::FileSizeComputation::is_cacheable`]).
    ///
    /// The list is mirrored in the extension and the CLI, which cannot import it, under a drift
    /// check (`scripts/test/engine-stage-coordination.test.mjs`).
    pub fn is_transient(stage: &str) -> bool {
        matches!(stage, TIMEOUT | PANIC | ENGINE_GONE)
    }
}

/// Stage names for the [`ImportDiagnostic`]s the engine emits on the *success* path.
///
/// A separate vocabulary from [`stage`]: these never become a `BundleFailure::stage` and so
/// never pass through `contract_stage`. They are constants for the other reason the failure
/// stages are — so the guard over `daemon/src/engine` can insist that no stage name anywhere
/// in the engine is a bare string literal, with no exceptions to carve out.
pub mod diagnostic_stage {
    /// A module Rolldown kept as an import boundary instead of bundling.
    pub const EXTERNAL: &str = "external";
    /// Bytes the graph pulled in that are NOT in the measured chunk — a stylesheet, almost always.
    ///
    /// The build **succeeds**. The size we report is the JS chunk, measured exactly; these bytes
    /// ship with the package and are not in it.
    ///
    /// The mechanism is the **plugin**, not the output shape. Rolldown 1.1.5 does not emit a CSS
    /// asset — it refuses the build outright at the LINK stage (`UNSUPPORTED_FEATURE: Bundling CSS
    /// is no longer supported`), so a package whose entry graph **imports** a stylesheet was
    /// `unmeasured_stage: Some("link")` and nobody saw it, because a failed build silently became a
    /// fabricated size. `plugin.rs` links the stylesheet as `ModuleType::Empty` and records its
    /// bytes here instead. (The earlier account of this — that a `.css` module became an emitted
    /// asset and the adapter's "no assets" guard then failed the build — was wrong: neutering
    /// `is_stylesheet` and running the build produces a link failure, never an asset.)
    ///
    /// **What this is NOT (measured 2026-07-14).** The trigger is an `import "./x.css"` reachable
    /// from the entry — not "the package ships a `.css` file". An earlier draft named swiper,
    /// react-datepicker and react-toastify as examples; **none of the three qualifies.** Their
    /// published JavaScript contains no reference to a stylesheet at all (the consumer is told to
    /// import the CSS themselves), and Import Lens never analyses that bare side-effect import,
    /// because a specifier with no default/named/namespace clause produces no `DetectedImport`. The
    /// real-package guard is `@uiw/react-md-editor`, whose ESM entry really does `import
    /// "./index.css"` (`daemon/tests/candidate_badges.rs`).
    ///
    /// **Confidence: an asset-emitting package is Medium by design.** It is not exempted as
    /// "disclosure only". A number that omits bytes the user's bundle really will carry is not a
    /// High-confidence measurement of that package's cost, and claiming otherwise is the same
    /// overclaim — one order of magnitude smaller — that this whole model exists to stop. It is
    /// also what the `external` diagnostic beside it already does for the same reason. Medium
    /// carries no `~` prefix in the UI (that is reserved for Low), so a correctly-measured CSS
    /// package reads as a plain number with a stated caveat, which is exactly what it is.
    pub const UNCOUNTED_ASSETS: &str = "uncounted_assets";
}

/// `stage` is one of [`stage::ALL`].
#[derive(Debug)]
pub struct BundleFailure {
    pub stage: String,
    pub message: String,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Modules that PARSED before the build gave up. Recorded at `module_parsed`, which is why it
    /// is not the right freshness set for a failure: the one module it can never contain is the one
    /// that broke.
    pub loaded_paths: Vec<PathBuf>,
    /// Fingerprints of every module whose bytes this build READ, captured in the plugin's `load`
    /// hook — so unlike `loaded_paths` this DOES include the module that failed to parse.
    ///
    /// A deterministic failure is cached (ADR-0006, invariant 3), and a cached fact must expire
    /// when the fact would change. These are the bytes the failure was derived from, so these are
    /// what it expires against. Empty for a failure that never entered the engine.
    pub read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
}
