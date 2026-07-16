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
/// This is now the FALLBACK shape only. A classified asset ([`CollectedAsset`]) is processed the
/// way it ships and counted (B2); an asset reaches here only when it could not be processed — a
/// Lightning CSS failure — or when Rolldown itself emitted one beside the chunk (nothing does
/// today). Disclosing those bytes is the honest alternative to counting bytes nothing rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncountedAsset {
    pub path: PathBuf,
    pub bytes: u64,
}

/// What a non-JavaScript module ships as, which decides how it is processed (B2).
///
/// CSS needs a processor (Lightning CSS resolves its `@import` tree and minifies it). A wasm or
/// font has none: its shipped size is its raw bytes, and compressing them is the whole answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AssetKind {
    Css,
    Wasm,
    Font,
}

impl AssetKind {
    /// Every kind, so a test can quantify over the whole vocabulary rather than the subset
    /// someone remembered.
    pub const ALL: &'static [Self] = &[Self::Css, Self::Wasm, Self::Font];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Css => "css",
            Self::Wasm => "wasm",
            Self::Font => "font",
        }
    }
}

/// A non-JavaScript module the graph imported, classified and intercepted at the load boundary so
/// the JavaScript chunk still measures exactly. Its bytes really do ship, so the pipeline processes
/// it the way it ships and folds the result into the Import Cost (B2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedAsset {
    pub path: PathBuf,
    pub kind: AssetKind,
    /// The file's size on disk. Used for the raw-byte fallback when processing fails, never as the
    /// shipped size of a stylesheet (which is only known after Lightning CSS bundles it).
    pub raw_bytes: u64,
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
    /// The classified non-JavaScript modules the graph imported, intercepted at the load boundary
    /// (see [`CollectedAsset`]). The pipeline processes these and folds their shipped bytes into
    /// the size (B2); they are NOT in `code`, which is the JavaScript chunk alone.
    pub assets: Vec<CollectedAsset>,
    /// Bytes this build knows about but cannot process: assets Rolldown itself emitted beside the
    /// chunk. Nothing does today, so this is normally empty; it is disclosed rather than counted
    /// because there is no file behind it to process.
    pub emitted_assets: Vec<UncountedAsset>,
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
    /// Every module the graph loaded, canonical and sorted. The memo needs these — not just
    /// the fingerprints — to find the first-party manifests that shaped resolution, exactly
    /// as the size path does (`analyze::manifest_augmented_fingerprints`).
    pub loaded_paths: Vec<PathBuf>,
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
    /// Declares the vocabulary. Each constant, its membership in [`ALL`], and — because `ALL` is
    /// ordered — its [`rank`] are emitted from the *same* line of the same invocation. So a stage
    /// that exists but is missing from `ALL` (and is therefore relabelled `generate` at the
    /// contract edge while `file_size.rs` passes it through untouched, one failure under two
    /// names), or one that exists with no place in the order, is not a mistake you can make here.
    macro_rules! stages {
        ($($(#[$attribute:meta])* $name:ident => $value:literal,)+) => {
            $($(#[$attribute])* pub const $name: &str = $value;)+

            /// Every stage declared above, **in rank order** — see [`rank`]. Anything absent from
            /// this list would collapse to [`GENERATE`] at the contract edge, and would have no
            /// rank; which is why the list is generated from the declarations instead of restated
            /// beside them.
            pub const ALL: &[&str] = &[$($name),+];
        };
    }

    // DECLARATION ORDER IS RANK ORDER. Adding a stage means deciding where the build reaches it,
    // and nothing else; see `rank`.
    stages! {
        // ---- The build did not happen. --------------------------------------------------------
        //
        // These three are not stages the build reached — they are the build being LOST, and they
        // preempt everything below because nothing below ever ran. Ranking them first is what makes
        // it impossible for a deterministic failure to outrank a transient one and so present a
        // scheduling accident as a fact about the package's bytes — the one that must be cacheable
        // and the other that must never be (ADR-0006, invariant 3).
        //
        // Today they cannot even compete: each is constructed in `boundary.rs` at a point where the
        // build's diagnostics do not exist (a panic unwinds straight past `classify_failure`; a
        // timeout drops the future), so each carries `diagnostics: Vec::new()` and no ranking is
        // performed. This order means that if that ever changes, the safe answer is the one that
        // wins by default rather than the one someone remembered to special-case.
        /// A build that unwound into the boundary's `catch_unwind`.
        PANIC => "panic",
        /// A build that did not finish within `boundary::BUILD_TIMEOUT` and was cancelled.
        /// That is the whole of its meaning: it says nothing about how long a *request* took,
        /// because a request no longer waits for every build it triggers (§9).
        TIMEOUT => "timeout",
        /// The engine runtime dropped the build without replying.
        ENGINE_GONE => "engine_gone",

        // ---- The build was abandoned. ----------------------------------------------------------
        //
        // Not a module's failure: a fact about the WHOLE build. The graph blew a hard limit
        // (`engine::limits`), so it was never going to complete, and under ADR-0006 that is the
        // reason the import has no size at all. A resolve error in one module of a graph that was
        // abandoned is shrapnel — reporting it would hand the user the symptom and hide the cause,
        // and the stage is what the user gets and what the cache stores.
        //
        // It ranks here rather than at the point the breach is DETECTED (the plugin's `load` hook,
        // i.e. between `resolve` and `parse`), and that is the correction: ranking it at load put it
        // behind `resolve`, so the declared order promised `resolve` would win a build that
        // `classify_failure` has always — correctly — answered `module_graph_limit`, by
        // short-circuiting on the recorded breach before any ranking runs. The rank was decorative
        // and it disagreed with the code, which is the same defect in the other direction: the SRS
        // derives the reported order FROM this list, so the list said something false.
        //
        // The breach is not produced by the ranking and cannot be: the limit is ours, enforced in
        // the plugin, and no Rolldown event kind maps to it (`adapter::stage_for`). The short-circuit
        // is its only producer. This rank is therefore what the order CLAIMS — the SRS's sentence,
        // and `contract_diagnostics`'s sort key — and it now claims what the code does.
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
        /// Linking, everything else — and the catch-all for a Rolldown event kind this contract
        /// has no name for, which is why it must rank AFTER the two link failures it would
        /// otherwise mask.
        LINK => "link",
        /// Generating the chunk.
        GENERATE => "generate",
        /// Inspecting what was generated: the build produced something other than one JS chunk.
        OUTPUT_SHAPE => "output_shape",
    }

    /// Where a stage sits in the order above. **The earliest one present is the one reported.**
    ///
    /// A failure stage is a durable, user-visible value — under ADR-0006 a failed build has no size
    /// at all, so the stage *is* the answer, and a deterministic one is cached against the bytes it
    /// was measured from. It therefore may not be decided by a race, and it was: Rolldown fans its
    /// module tasks out onto the async runtime and accumulates their diagnostics **in the order the
    /// tasks report**, so the adapter's old "first diagnostic that is not `link`" picked whichever
    /// module happened to finish first. A build with a parse error in one module and an unresolved
    /// import in another answered `parse` on one run and `resolve` on the next, for byte-identical
    /// inputs — measured at 38/10 over 48 runs (`daemon/tests/engine_failure_stage.rs`).
    ///
    /// **Earliest wins, and the order is the pipeline's, not a severity ladder.** The earliest
    /// failure is the likeliest ROOT CAUSE — a module that failed to resolve is often *why*
    /// something downstream is malformed, and the later diagnostics are frequently its shrapnel —
    /// and, unlike a hand-picked severity order, it needs no judgement call to maintain: a new stage
    /// is ranked by where the build reaches it. We do not claim to know which failure a user would
    /// rather hear about.
    ///
    /// **Four stages are not ranked by where the build reaches them, because the build never reached
    /// them.** The three transients are the build being LOST and `module_graph_limit` is the build
    /// being ABANDONED; none is a thing that happened *to a module*, and each is the reason there is
    /// no answer at all. They lead the order for that reason, not because they occur early — the
    /// breach is in fact *detected* in the `load` hook, after resolve. Ranking a module's diagnostic
    /// ahead of one of them would report the shrapnel of a build that was never going to finish, and
    /// hide the cause. Every other stage is a position in a build that was genuinely running.
    ///
    /// A stage outside the vocabulary sorts last. `adapter::stage_for` can only return a declared
    /// one, so that arm is unreachable from the ranking's only caller; it is here so the order is
    /// total rather than partial.
    pub fn rank(stage: &str) -> usize {
        ALL.iter()
            .position(|known| *known == stage)
            .unwrap_or(ALL.len())
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
