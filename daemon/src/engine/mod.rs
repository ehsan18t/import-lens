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
    pub contributions: Vec<ModuleContribution>,
    pub exported_names: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub matched_side_effect_paths: Vec<PathBuf>,
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
