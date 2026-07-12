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

/// `stage` is one of: "resolve" | "parse" | "link" | "generate" |
/// "output_shape" | "module_graph_limit" | "missing_export" |
/// "ambiguous_export".
#[derive(Debug)]
pub struct BundleFailure {
    pub stage: String,
    pub message: String,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub loaded_paths: Vec<PathBuf>,
}
