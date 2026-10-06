//! The Rolldown bundling engine. This surface is Import Lens-owned: no Rolldown type may appear in
//! any public field, argument, or return type, and only `adapter.rs`/`plugin.rs` may import the
//! crate.

mod adapter;
mod asset_classifier;
mod asset_input;
pub mod boundary;
pub(crate) mod dependency_paths;
mod entry;
pub(crate) mod limits;
mod plugin;
pub(crate) mod scheduling;

use std::path::PathBuf;

pub use crate::ipc::protocol::ImportRuntime;
pub use adapter::RolldownEngine;
pub(crate) use asset_classifier::{AssetClass, classify_asset_class};
pub use asset_input::CollectedAsset;
#[cfg(test)]
pub(crate) use asset_input::read_collected_asset;

#[derive(Debug, Clone)]
pub struct BundleRequest {
    pub entries: Vec<BundleEntry>,
    pub runtime: ImportRuntime,
    pub purpose: BundlePurpose,
}

/// An entry to measure. It carries **no `sideEffects` metadata** by contract: Rolldown reads the
/// package's `sideEffects` itself, from the manifest the plugin supplies, and is the only authority
/// on retention (FR-021). The daemon's own reading of the field decides a badge, never a byte, so
/// it stays on the pipeline's side of this boundary.
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

/// A non-JavaScript module whose bytes ship with the package but are NOT in the measured size.
///
/// This is the fallback shape. A classified asset ([`CollectedAsset`]) is processed and counted; an
/// asset lands here only when it could not be processed (a Lightning CSS failure) or when Rolldown
/// emitted one beside the chunk (nothing does today). Its raw bytes are disclosed, not counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncountedAsset {
    pub path: PathBuf,
    pub bytes: u64,
}

/// The one sentence every uncounted-asset disclosure uses, shared by the engine adapter (an asset
/// Rolldown emitted) and the asset pipeline (a processor fallback, or an unmeasured kind).
///
/// The total is qualified whenever it may understate the shortfall: an asset whose bytes could not
/// be stat'd contributes 0, and `total_is_floor` says the listed rows are not everything missing (a
/// resource limit stopped the walk before the files they reach).
pub fn uncounted_assets_message(assets: &[UncountedAsset], total_is_floor: bool) -> String {
    let disclosed_bytes: u64 = assets.iter().map(|asset| asset.bytes).sum();
    let total = if !total_is_floor && assets.iter().all(|asset| asset.bytes > 0) {
        format!("totalling {disclosed_bytes} bytes")
    } else if disclosed_bytes > 0 {
        format!("totalling at least {disclosed_bytes} bytes")
    } else {
        "of unknown size".to_owned()
    };
    let names = assets
        .iter()
        .map(|asset| {
            asset
                .path
                .file_name()
                .unwrap_or(asset.path.as_os_str())
                .to_string_lossy()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "package ships {} non-JavaScript asset(s) {total} that this size does NOT include: {names}",
        assets.len()
    )
}

/// What a non-JavaScript module ships as, which decides how it is processed.
///
/// CSS needs a processor (Lightning CSS resolves its `@import` tree and minifies it). A wasm or
/// font has none: its shipped size is its raw bytes, compressed.
///
/// This crosses the wire inside [`crate::ipc::protocol::AssetContribution`], so the snake_case
/// spellings are the contract the extension matches on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Css,
    Wasm,
    Font,
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
    /// Source bytes admitted under this build's aggregate graph ceiling. Direct assets contribute
    /// their exact raw length even though Rolldown sees them as empty modules.
    pub graph_source_bytes: usize,
    pub loaded_paths: Vec<PathBuf>,
    /// Fingerprints captured when each module's bytes were read during this build,
    /// so freshness describes the bytes the size was actually measured from (§8.3).
    pub read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    /// Loaded paths with no read-time fingerprint — binary modules the plugin handed
    /// back to Rolldown. The caller fingerprints these by reading them.
    pub unhashed_paths: Vec<PathBuf>,
    pub contributions: Vec<ModuleContribution>,
    /// The chunk's public export list. It has no production reader, but the qualification suites
    /// use it to assert that every requested `__il_entry_*` alias survived linking, the invariant
    /// the selection mechanism rests on (§8.4). Do not remove it.
    pub exported_names: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// The classified non-JavaScript modules the graph imported, intercepted at the load boundary
    /// (see [`CollectedAsset`]). The pipeline processes these and folds their shipped bytes into
    /// the size; they are NOT in `code`, which is the JavaScript chunk alone.
    pub assets: Vec<CollectedAsset>,
    /// Bytes this build knows about but cannot process: assets Rolldown itself emitted beside the
    /// chunk. Nothing does today, so this is normally empty; it is disclosed rather than counted
    /// because there is no file behind it to process.
    pub emitted_assets: Vec<UncountedAsset>,
}

/// The result of export enumeration (§8.4).
///
/// Carries the warnings of a build that succeeded, and the build's read-time fingerprints, which
/// let the caller memoize the enumeration instead of building the whole package graph on every
/// completion popup.
#[derive(Debug, Clone)]
pub struct ExportEnumeration {
    pub names: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    /// Every module the graph loaded, canonical and sorted. The memo needs these, not just the
    /// fingerprints, to find the first-party manifests that shaped resolution, as the size path
    /// does (`analyze::manifest_augmented_fingerprints`).
    pub loaded_paths: Vec<PathBuf>,
    /// Loaded paths with no read-time fingerprint. A non-empty list means the
    /// enumeration must not be memoized: there is nothing to expire it against.
    pub unhashed_paths: Vec<PathBuf>,
}

/// The closed vocabulary of `BundleFailure::stage` (§12).
///
/// This module is the single source of truth. `pipeline::analyze::contract_stage` derives its
/// mapping from [`stage::ALL`], so a stage cannot exist here and be relabelled `generate` on the
/// way to the client. Every `BundleFailure` takes its stage from one of these constants; a guard
/// over `daemon/src/engine` rejects a bare string literal at a construction site.
pub mod stage {
    /// Declares the vocabulary. Each constant, its membership in [`ALL`], and (because `ALL` is
    /// ordered) its [`rank`] come from the same line, so a stage cannot exist without a place in
    /// `ALL` and in the order.
    macro_rules! stages {
        ($($(#[$attribute:meta])* $name:ident => $value:literal,)+) => {
            $($(#[$attribute])* pub const $name: &str = $value;)+

            /// Every stage declared above, **in rank order** (see [`rank`]). Anything absent from
            /// this list would collapse to [`GENERATE`] at the contract edge and have no rank.
            pub const ALL: &[&str] = &[$($name),+];
        };
    }

    // DECLARATION ORDER IS RANK ORDER. Adding a stage means deciding where the build reaches it,
    // and nothing else; see `rank`.
    stages! {
        // ---- The build produced no reusable answer. ------------------------------------------
        //
        // The first three mean the build was LOST; the fourth means a supported asset's exact bytes
        // could not be observed. All four preempt a deterministic module diagnostic, because
        // presenting a request-local failure as a fact about the package's bytes would make it
        // durable (ADR-0006, invariant 3). Today each is built in `boundary.rs` with no
        // diagnostics, so no ranking runs; this order makes the safe answer win if that changes.
        /// A build that unwound into the boundary's `catch_unwind`.
        PANIC => "panic",
        /// A build that did not finish within `boundary::BUILD_TIMEOUT` and was cancelled. It says
        /// nothing about how long a *request* took: a request does not wait for every build it
        /// triggers (§9).
        TIMEOUT => "timeout",
        /// The engine runtime dropped the build without replying.
        ENGINE_GONE => "engine_gone",
        /// A supported asset input could not be observed as exact readable bytes. A concurrent
        /// install, file lock, permission blip, or missing file can all recover without any input
        /// the failed build fingerprinted changing.
        ASSET_IO => "asset_io",

        // ---- The build was abandoned. ----------------------------------------------------------
        //
        // A fact about the WHOLE build: the graph blew a hard limit (`engine::limits`), and under
        // ADR-0006 that is why the import has no size. A resolve error in an abandoned graph is
        // shrapnel; reporting it would hide the cause.
        //
        // It ranks here, not where the breach is detected (the plugin's `load` hook, after
        // `resolve`), because `classify_failure` short-circuits on a recorded breach before any
        // ranking runs and no Rolldown event maps to it (`adapter::stage_for`). The SRS and
        // `contract_diagnostics` derive the reported order from this list, so it must match that.
        /// The module graph breached a hard limit (2,000 modules, 20 MiB per module source, 100 MiB
        /// total), so the build was abandoned rather than completed on a partial graph.
        MODULE_GRAPH_LIMIT => "module_graph_limit",

        // ---- The build's own stages, in the order the build reaches them. ----------------------
        /// Resolving a module's dependencies, before anything is read.
        RESOLVE => "resolve",
        /// Parsing and transforming a module's source.
        PARSE => "parse",
        /// Linking: a requested export that no module provides.
        MISSING_EXPORT => "missing_export",
        /// Linking: a name two star providers both claim.
        AMBIGUOUS_EXPORT => "ambiguous_export",
        /// Linking, everything else, and the catch-all for an unnamed Rolldown event kind. It must
        /// rank AFTER the two link failures it would otherwise mask.
        LINK => "link",
        /// Generating the chunk.
        GENERATE => "generate",
        /// Inspecting what was generated: the build produced something other than one JS chunk.
        OUTPUT_SHAPE => "output_shape",
    }

    /// Where a stage sits in the order above. **The earliest one present is the one reported.**
    ///
    /// A failure stage is a durable, user-visible value (under ADR-0006 a failed build has no size,
    /// and a deterministic failure is cached), so it may not be decided by a race. Rolldown reports
    /// module diagnostics in task-completion order, which varies between runs on identical inputs
    /// (`daemon/tests/engine_failure_stage.rs`); ranking makes the reported stage deterministic.
    ///
    /// The order is the pipeline's, not a severity ladder: the earliest failure is the likeliest
    /// root cause, and a new stage is ranked by where the build reaches it. The five leading
    /// outcomes (`panic`, `timeout`, `engine_gone`, `asset_io`, `module_graph_limit`) rank by
    /// whole-build meaning instead: each is the reason no reusable answer exists.
    ///
    /// A stage outside the vocabulary sorts last, so the order is total.
    pub fn rank(stage: &str) -> usize {
        ALL.iter()
            .position(|known| *known == stage)
            .unwrap_or(ALL.len())
    }
}

/// Stage names for the [`ImportDiagnostic`]s the engine emits on the *success* path.
///
/// Separate from [`stage`]: these never become a `BundleFailure::stage`. They are constants so the
/// guard over `daemon/src/engine` can forbid every bare stage-name literal without exceptions.
pub mod diagnostic_stage {
    /// A module Rolldown kept as an import boundary instead of bundling.
    pub const EXTERNAL: &str = "external";
    /// Non-JavaScript bytes the import ships that are NOT in the measured size.
    ///
    /// The build **succeeds**; these bytes are disclosed beside the number. Rolldown refuses to
    /// bundle CSS (it fails the build at LINK), so `plugin.rs` stubs every classified asset as
    /// `ModuleType::Empty` and the pipeline processes it; this stage covers what could not be
    /// processed or is outside the counted taxonomy.
    ///
    /// The trigger is an `import "./x.css"` reachable from the entry, not "the package ships a
    /// `.css` file": a bare side-effect import in the user's own code produces no `DetectedImport`.
    /// The real-package guard is `@uiw/react-md-editor` (`daemon/tests/candidate_badges.rs`).
    ///
    /// **Confidence is Medium by design**, as for `external`: a number that omits bytes the user's
    /// bundle will carry is not High confidence. Medium carries no `~` prefix in the UI (that is
    /// reserved for Low), so it reads as a plain number with a stated caveat.
    pub const UNCOUNTED_ASSETS: &str = "uncounted_assets";
    /// Assets that ARE in the number, but whose bytes may be counted more than once.
    ///
    /// The stylesheet set bundles into ONE artifact because that is how it ships, and that union is
    /// what dedupes an `@import` two sheets share. The union is all-or-nothing, so when it fails the
    /// set is retried one sheet at a time: every sheet is still counted (nothing is
    /// [`UNCOUNTED_ASSETS`]), but bytes two sheets share are inlined into both and counted twice,
    /// so the size reads **high**. Without this stage that over-count would read as High
    /// confidence and be cached as durable.
    pub const IMPRECISE_ASSETS: &str = "imprecise_assets";

    /// Every diagnostic stage declared above. Add a constant, add it here.
    ///
    /// `pipeline::stage` decides durability from an allowlist, so an unlisted stage makes every
    /// package carrying it permanently uncacheable, and the property test over stage
    /// classification cannot catch it because it quantifies across this list.
    pub const ALL: &[&str] = &[EXTERNAL, UNCOUNTED_ASSETS, IMPRECISE_ASSETS];
}

/// `stage` is one of [`stage::ALL`].
#[derive(Debug)]
pub struct BundleFailure {
    pub stage: String,
    pub message: String,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Modules that PARSED before the build gave up (recorded at `module_parsed`). Not a freshness
    /// set for a failure: it can never contain the module that broke.
    pub loaded_paths: Vec<PathBuf>,
    /// Fingerprints of every module whose bytes this build READ, captured in the plugin's `load`
    /// hook, so unlike `loaded_paths` this includes the module that failed to parse.
    ///
    /// A deterministic failure is cached (ADR-0006, invariant 3) and expires against these bytes.
    /// Empty for a failure that never entered the engine.
    pub read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
}
