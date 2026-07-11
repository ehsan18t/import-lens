//! Rolldown adapter (spec §7/§8). This file and `plugin.rs` are the only
//! places allowed to import the `rolldown` crate family; every public
//! surface translates to the contract types in `mod.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rolldown::plugin::Pluginable;
use rolldown::{
    Bundler, BundlerOptions, CodeSplittingMode, InputItem, IsExternal, OutputFormat, Platform,
    PreserveEntrySignatures, RawMinifyOptions, ResolveOptions,
};
use rolldown_common::{Output, OutputChunk};
use rolldown_error::{BuildDiagnostic, EventKind};

use super::plugin::{BuildState, ImportLensPlugin};
use super::{
    BundleArtifact, BundleFailure, BundleRequest, ImportDiagnostic, ImportRuntime,
    ModuleContribution, entry,
};
use crate::pipeline::node_builtins::NODE_BUILTIN_MODULES;
use crate::pipeline::resolver::resolve_options as shared_resolve_options;

/// Stateless adapter; one Rolldown bundler is built per request and never
/// reused across builds.
pub struct RolldownEngine;

impl RolldownEngine {
    /// Must be polled inside a Tokio runtime: Rolldown spawns its module
    /// tasks through the ambient handle.
    pub async fn bundle(&self, request: BundleRequest) -> Result<BundleArtifact, BundleFailure> {
        let Some(first_entry) = request.entries.first() else {
            return Err(BundleFailure {
                stage: "generate".to_owned(),
                message: "bundle request contains no entries".to_owned(),
                diagnostics: Vec::new(),
                loaded_paths: Vec::new(),
            });
        };
        let input = InputItem {
            name: None,
            import: entry::VIRTUAL_ENTRY_ID.to_owned(),
        };
        let options = build_options(input, first_entry.package_root.clone(), request.runtime);
        let plugin = ImportLensPlugin::for_request(&request);
        let state = plugin.state();
        let output = run_build(options, plugin, &state).await?;
        translate(output, &state, &request)
    }

    /// Export enumeration (§8.4): the resolved real entry becomes the strict
    /// entry and the chunk's public export list is the answer.
    pub async fn enumerate_exports(
        &self,
        entry_path: PathBuf,
        runtime: ImportRuntime,
    ) -> Result<Vec<String>, BundleFailure> {
        let cwd = entry_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let input = InputItem {
            name: None,
            import: rolldown_entry_path(&entry_path),
        };
        let options = build_options(input, cwd, runtime);
        let plugin = ImportLensPlugin::passthrough();
        let state = plugin.state();
        let output = run_build(options, plugin, &state).await?;
        let chunk = single_chunk(&output, &state)?;
        Ok(chunk.exports.iter().map(|name| name.to_string()).collect())
    }
}

fn rolldown_entry_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if let Some(unc) = normalized.strip_prefix("//?/UNC/") {
        return format!("//{unc}");
    }
    normalized
        .strip_prefix("//?/")
        .unwrap_or(&normalized)
        .to_owned()
}

/// Fixed build options (spec §7.1). Everything not set here intentionally
/// keeps Rolldown's default: tree-shaking enabled with default annotations,
/// source maps off.
fn build_options(input: InputItem, cwd: PathBuf, runtime: ImportRuntime) -> BundlerOptions {
    BundlerOptions {
        input: Some(vec![input]),
        cwd: Some(cwd),
        external: Some(builtin_external()),
        format: Some(OutputFormat::Esm),
        // Strict signatures keep every requested `__il_entry_*` alias alive
        // verbatim in the chunk's export list.
        preserve_entry_signatures: Some(PreserveEntrySignatures::Strict),
        // Bool(false) inlines dynamic imports into the single chunk; 1.1.5
        // has no separate inline-dynamic-imports option.
        code_splitting: Some(CodeSplittingMode::Bool(false)),
        // None is NOT off in 1.1.5 — it normalizes to dead-code-elimination
        // minification. The raw chunk must stay byte-faithful (§8.1).
        minify: Some(RawMinifyOptions::Bool(false)),
        // An UNSET platform is derived from the format, and `Esm` derives
        // `Platform::Browser` — which makes Rolldown append `browser` to the
        // condition list on top of ours (rolldown_resolver::ResolverConfig::build)
        // and inject a `process.env.NODE_ENV` define. Both corrupt measurement: the
        // Server runtime would resolve a package's `browser` export condition, and
        // the define would dead-code-eliminate branches the runtime keeps.
        // `Neutral` appends nothing, leaving the per-runtime condition list from the
        // shared resolver authoritative (§7.1).
        platform: Some(Platform::Neutral),
        resolve: Some(mapped_resolve_options(runtime)),
        ..BundlerOptions::default()
    }
}

/// Node builtins stay external (§7.1); Rolldown matches these against the
/// raw specifier by exact string equality.
fn builtin_external() -> IsExternal {
    let mut specifiers = Vec::with_capacity(NODE_BUILTIN_MODULES.len() * 2);
    for module in NODE_BUILTIN_MODULES {
        specifiers.push((*module).to_owned());
        specifiers.push(format!("node:{module}"));
    }
    IsExternal::from(specifiers)
}

/// The direct resolver's per-runtime configuration is the single source of
/// truth; mirroring it field-by-field keeps the two resolution surfaces from
/// drifting (§7.1).
fn mapped_resolve_options(runtime: ImportRuntime) -> ResolveOptions {
    let shared = shared_resolve_options(runtime);
    ResolveOptions {
        alias_fields: Some(shared.alias_fields),
        condition_names: Some(shared.condition_names),
        extensions: Some(shared.extensions),
        extension_alias: Some(shared.extension_alias),
        main_fields: Some(shared.main_fields),
        ..ResolveOptions::default()
    }
}

async fn run_build(
    options: BundlerOptions,
    plugin: ImportLensPlugin,
    state: &BuildState,
) -> Result<rolldown::BundleOutput, BundleFailure> {
    let mut bundler = Bundler::with_plugins(options, vec![Arc::new(plugin) as Arc<dyn Pluginable>])
        .map_err(|error| classify_failure(error.into_vec(), state))?;
    let result = bundler.generate().await;
    // Release plugin-driver resources even when the build failed.
    let _ = bundler.close().await;
    result.map_err(|error| classify_failure(error.into_vec(), state))
}

fn translate(
    output: rolldown::BundleOutput,
    state: &BuildState,
    request: &BundleRequest,
) -> Result<BundleArtifact, BundleFailure> {
    let chunk = single_chunk(&output, state)?;

    let mut contributions = Vec::new();
    for (id, module) in chunk.modules.keys.iter().zip(chunk.modules.values.iter()) {
        if id.as_str() == entry::VIRTUAL_ENTRY_ID {
            continue;
        }
        // Runtime-only virtual modules and externals have no real path.
        let Some(path) = id.as_path() else {
            continue;
        };
        let rendered_bytes = module.rendered_length();
        if rendered_bytes == 0 {
            continue;
        }
        contributions.push(ModuleContribution {
            path: path.to_path_buf(),
            rendered_bytes,
        });
    }

    let mut diagnostics = warning_diagnostics(&output.warnings);
    for import in &chunk.imports {
        diagnostics.push(ImportDiagnostic {
            stage: "external".to_owned(),
            message: format!("external module kept as an import boundary: {import}"),
        });
    }
    if request
        .entries
        .iter()
        .any(|entry| entry.reported_side_effects.is_array())
    {
        // §7.4: matched side-effect paths would need Rolldown to expose its retention
        // decisions, which its public output does not. Reporting stays conservative
        // rather than re-implementing a matcher.
        //
        // On Windows the bundler's own glob matching does not fire at all (it compares
        // backslashed relative paths), so glob-declared effectful modules are
        // over-shaken and the reported size is too SMALL. Saying only that "confidence
        // is conservative" reads as an over-estimate — the opposite of the truth — so
        // name the direction of the error on the platform where it happens.
        let message = if cfg!(windows) {
            "package sideEffects globs present; on Windows the bundler cannot match them, \
             so effectful modules may be tree-shaken away and this size may be UNDERCOUNTED"
        } else {
            "package sideEffects globs present; matched paths are unavailable from public \
             bundler metadata, so side-effect confidence is conservative"
        };
        diagnostics.push(ImportDiagnostic {
            stage: "side_effects".to_owned(),
            message: message.to_owned(),
        });
    }

    let (read_time_fingerprints, unhashed_paths) = state.read_time_fingerprints();

    Ok(BundleArtifact {
        code: chunk.code.clone(),
        loaded_paths: state.sorted_loaded_paths(),
        read_time_fingerprints,
        unhashed_paths,
        contributions,
        exported_names: chunk.exports.iter().map(|name| name.to_string()).collect(),
        diagnostics,
        matched_side_effect_paths: Vec::new(),
    })
}

/// The build must produce exactly one JavaScript chunk and nothing else; any
/// other shape is a typed `output_shape` failure (§7.1).
fn single_chunk(
    output: &rolldown::BundleOutput,
    state: &BuildState,
) -> Result<Arc<OutputChunk>, BundleFailure> {
    let mut chunks = Vec::new();
    let mut asset_names = Vec::new();
    for item in &output.assets {
        match item {
            Output::Chunk(chunk) => chunks.push(Arc::clone(chunk)),
            Output::Asset(asset) => asset_names.push(asset.filename.to_string()),
        }
    }
    if chunks.len() != 1 || !asset_names.is_empty() {
        return Err(BundleFailure {
            stage: "output_shape".to_owned(),
            message: format!(
                "expected exactly one chunk and no assets, got {} chunk(s) and {} asset(s){}",
                chunks.len(),
                asset_names.len(),
                if asset_names.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", asset_names.join(", "))
                }
            ),
            diagnostics: warning_diagnostics(&output.warnings),
            loaded_paths: state.sorted_loaded_paths(),
        });
    }
    Ok(chunks.remove(0))
}

fn classify_failure(diagnostics: Vec<BuildDiagnostic>, state: &BuildState) -> BundleFailure {
    let loaded_paths = state.sorted_loaded_paths();
    if let Some(breach) = state.take_breach() {
        return BundleFailure {
            stage: "module_graph_limit".to_owned(),
            message: breach,
            diagnostics: error_diagnostics(&diagnostics),
            loaded_paths,
        };
    }

    let stage = diagnostics
        .iter()
        .map(stage_for)
        .find(|stage| *stage != "link")
        .unwrap_or("link");
    let message = if diagnostics.is_empty() {
        "rolldown build failed without diagnostics".to_owned()
    } else {
        diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };

    BundleFailure {
        stage: stage.to_owned(),
        message,
        diagnostics: error_diagnostics(&diagnostics),
        loaded_paths,
    }
}

fn stage_for(diagnostic: &BuildDiagnostic) -> &'static str {
    match diagnostic.kind() {
        EventKind::MissingExportError => {
            // 1.1.5 reports a name lost to conflicting star providers through
            // the missing-export path; keep the contract's distinction (§12).
            if diagnostic
                .to_string()
                .to_ascii_lowercase()
                .contains("ambiguous")
            {
                "ambiguous_export"
            } else {
                "missing_export"
            }
        }
        EventKind::AmbiguousExternalNamespaceError => "ambiguous_export",
        EventKind::ParseError | EventKind::JsonParseError | EventKind::TransformError => "parse",
        EventKind::UnresolvedEntry
        | EventKind::UnresolvedImport
        | EventKind::ResolveError
        | EventKind::UnloadableDependencyError => "resolve",
        _ => "link",
    }
}

/// Diagnostics cross the contract as plain strings only (§5.1): the stable
/// machine code plus the rendered message, never a Rolldown type or Debug
/// representation.
fn error_diagnostics(diagnostics: &[BuildDiagnostic]) -> Vec<ImportDiagnostic> {
    diagnostics
        .iter()
        .map(|diagnostic| ImportDiagnostic {
            stage: stage_for(diagnostic).to_owned(),
            message: format!("{}: {}", diagnostic.kind(), diagnostic),
        })
        .collect()
}

fn warning_diagnostics(warnings: &[BuildDiagnostic]) -> Vec<ImportDiagnostic> {
    warnings
        .iter()
        .map(|warning| ImportDiagnostic {
            stage: "generate".to_owned(),
            message: format!("{}: {}", warning.kind(), warning),
        })
        .collect()
}
