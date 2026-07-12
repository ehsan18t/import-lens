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
pub use crate::pipeline::resolver::SideEffectsMode;
pub use adapter::RolldownEngine;

#[derive(Debug, Clone)]
pub struct BundleRequest {
    pub entries: Vec<BundleEntry>,
    pub runtime: ImportRuntime,
    pub purpose: BundlePurpose,
}

#[derive(Debug, Clone)]
pub struct BundleEntry {
    /// Pre-resolved absolute entry file; the engine never re-resolves the
    /// bare package specifier.
    pub entry_path: PathBuf,
    pub package_root: PathBuf,
    pub selection: BundleSelection,
    pub reported_side_effects: SideEffectsMode,
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
    /// deadline, unwound, or lost its runtime tells us nothing about the package — and the
    /// static fallback the pipeline substitutes carries `error: None` and a plausible-looking
    /// byte count, which is exactly the shape a cache happily stores. Store it once and a
    /// healthy 17 KB package reports its 58-byte barrel for a whole cache generation.
    ///
    /// So every cache and memo the daemon writes gates on this: see
    /// `service::should_cache_result` and `service::file_size_is_cacheable`.
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
    /// The package declares `sideEffects` as an array and the matched paths are not
    /// recoverable from public bundler metadata, so confidence stays conservative.
    pub const SIDE_EFFECTS: &str = "side_effects";
}

/// `stage` is one of [`stage::ALL`].
#[derive(Debug)]
pub struct BundleFailure {
    pub stage: String,
    pub message: String,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub loaded_paths: Vec<PathBuf>,
}
