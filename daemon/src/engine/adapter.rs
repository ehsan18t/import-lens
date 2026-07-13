//! Rolldown adapter (spec §7/§8). This file and `plugin.rs` are the only
//! places allowed to import the `rolldown` crate family; every public
//! surface translates to the contract types in `mod.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rolldown::plugin::Pluginable;
use rolldown::{
    AttachDebugInfo, Bundler, BundlerOptions, CodeSplittingMode, ExperimentalOptions, InputItem,
    IsExternal, OutputFormat, Platform, PreserveEntrySignatures, RawMinifyOptions, ResolveOptions,
};
use rolldown_common::{Output, OutputChunk};
use rolldown_error::{BuildDiagnostic, EventKind};

use super::ExportEnumeration;
use super::plugin::{BuildState, ImportLensPlugin};
use super::{
    BundleArtifact, BundleFailure, BundleRequest, ImportDiagnostic, ImportRuntime,
    ModuleContribution, UncountedAsset, diagnostic_stage, entry, stage,
};
use crate::pipeline::node_builtins::{NODE_BUILTIN_MODULES, NODE_PREFIX_ONLY_MODULES};
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
                stage: stage::GENERATE.to_owned(),
                message: "bundle request contains no entries".to_owned(),
                diagnostics: Vec::new(),
                loaded_paths: Vec::new(),
                read_time_fingerprints: Vec::new(),
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
    ) -> Result<ExportEnumeration, BundleFailure> {
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
        let (read_time_fingerprints, unhashed_paths) = state.read_time_fingerprints();

        Ok(ExportEnumeration {
            names: chunk.exports.iter().map(|name| name.to_string()).collect(),
            // A successful build's warnings used to be dropped on the floor.
            diagnostics: warning_diagnostics(&output.warnings),
            read_time_fingerprints,
            unhashed_paths,
        })
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
        // Rolldown normalizes an UNSET attach_debug_info to `Simple`, which wraps
        // every rendered module in `//#region <id>` / `//#endregion` comments. Those
        // bytes land in `raw_bytes`, and `RenderedModule::rendered_length` sums every
        // source in a module's vec — the wrappers included — so they are also charged
        // inside the per-module contributions. Bundler metadata billed to the user as
        // package cost (§8.1/§8.2).
        experimental: Some(ExperimentalOptions {
            attach_debug_info: Some(AttachDebugInfo::None),
            ..ExperimentalOptions::default()
        }),
        resolve: Some(mapped_resolve_options(runtime)),
        ..BundlerOptions::default()
    }
}

/// Node builtins stay external (§7.1); Rolldown matches these against the
/// raw specifier by exact string equality.
fn builtin_external() -> IsExternal {
    let mut specifiers =
        Vec::with_capacity(NODE_BUILTIN_MODULES.len() * 2 + NODE_PREFIX_ONLY_MODULES.len());
    for module in NODE_BUILTIN_MODULES {
        specifiers.push((*module).to_owned());
        specifiers.push(format!("node:{module}"));
    }
    // Prefix-only builtins carry their `node:` prefix already, and must NOT be added
    // bare: the bare spelling belongs to whatever npm package owns that name.
    for module in NODE_PREFIX_ONLY_MODULES {
        specifiers.push((*module).to_owned());
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

    let uncounted = uncounted_assets(&output, state);
    let mut diagnostics = warning_diagnostics(&output.warnings);
    diagnostics.extend(uncounted_assets_diagnostic(&uncounted));
    for import in &chunk.imports {
        diagnostics.push(ImportDiagnostic {
            stage: diagnostic_stage::EXTERNAL.to_owned(),
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
        // Deliberately does NOT claim the size is undercounted on Windows. That claim
        // came from matrix rows 42/43, whose `#[ignore]` blames "backslashed relative
        // paths" — but Rolldown matches through `fast_glob`, which uses
        // `std::path::is_separator` and accepts `\` for a pattern's `/` on Windows, and
        // those fixtures' pattern (`fx.js`, at the package root) contains no separator
        // at all. The rows fail for an unrelated reason. Do not assert a direction of
        // error that has not been reproduced.
        diagnostics.push(ImportDiagnostic {
            stage: diagnostic_stage::SIDE_EFFECTS.to_owned(),
            message: "package sideEffects globs present; matched paths are unavailable from \
                      public bundler metadata, so side-effect confidence is conservative"
                .to_owned(),
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
        uncounted_assets: uncounted,
    })
}

/// The build must produce exactly one JavaScript **chunk** (§7.1). More than one means Rolldown
/// code-split the graph, and we measure a chunk — so a size taken from one of several would
/// under-report the package by however much is in the others. That is a typed `output_shape`
/// failure and stays one.
///
/// An emitted **asset** is not an output shape failure. The guard used to demand "one chunk and no
/// assets", which is a rule with no upside: an asset beside the chunk does not make the chunk wrong,
/// it makes it incomplete, and [`uncounted_assets_diagnostic`] says so without destroying the
/// measurement. Rolldown 1.1.5 in fact emits no asset for a stylesheet — it fails the build at the
/// LINK stage, and `plugin.rs` is what handles that (FR-018a) — so today this arm counts only what a
/// future Rolldown, or a plugin, might emit. It stays because "no assets" is not the invariant; "one
/// chunk" is.
fn single_chunk(
    output: &rolldown::BundleOutput,
    state: &BuildState,
) -> Result<Arc<OutputChunk>, BundleFailure> {
    let mut chunks = Vec::new();
    for item in &output.assets {
        if let Output::Chunk(chunk) = item {
            chunks.push(Arc::clone(chunk));
        }
    }
    if chunks.len() != 1 {
        return Err(BundleFailure {
            stage: stage::OUTPUT_SHAPE.to_owned(),
            message: format!(
                "expected exactly one JavaScript chunk, got {}; a split graph cannot be measured \
                 from one chunk without under-reporting the rest",
                chunks.len()
            ),
            diagnostics: warning_diagnostics(&output.warnings),
            loaded_paths: state.sorted_loaded_paths(),
            read_time_fingerprints: Vec::new(),
        });
    }
    Ok(chunks.remove(0))
}

/// Every byte this build knows about and did NOT count, named and totalled.
///
/// Two sources, because there are two ways a non-JS byte can exist here. The stylesheets the graph
/// imported, which the plugin linked as empty modules (Rolldown 1.1.5 cannot bundle CSS: left to
/// it, the whole build fails at the LINK stage). And anything Rolldown itself emitted beside the
/// chunk — **nothing does today**, CSS included, but the output-shape guard no longer treats one as
/// fatal, so it must not be silent either.
///
/// These bytes ship with the package and are not in the reported size, so the user is owed the
/// number. See [`diagnostic_stage::UNCOUNTED_ASSETS`] for why disclosing them costs the result its
/// High confidence rather than being exempted.
fn uncounted_assets(output: &rolldown::BundleOutput, state: &BuildState) -> Vec<UncountedAsset> {
    let mut assets = state
        .sorted_uncounted_assets()
        .into_iter()
        .map(|(path, bytes)| UncountedAsset { path, bytes })
        .collect::<Vec<_>>();

    assets.extend(output.assets.iter().filter_map(|item| match item {
        Output::Asset(asset) => Some(UncountedAsset {
            path: PathBuf::from(asset.filename.to_string()),
            bytes: asset.source.as_bytes().len() as u64,
        }),
        Output::Chunk(_) => None,
    }));

    assets
}

fn uncounted_assets_diagnostic(assets: &[UncountedAsset]) -> Option<ImportDiagnostic> {
    if assets.is_empty() {
        return None;
    }

    let total_bytes: u64 = assets.iter().map(|asset| asset.bytes).sum();
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

    Some(ImportDiagnostic {
        stage: diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
        message: format!(
            "package ships {} non-JavaScript asset(s) totalling {total_bytes} bytes that this \
             size does NOT include: {names}",
            assets.len()
        ),
    })
}

fn classify_failure(diagnostics: Vec<BuildDiagnostic>, state: &BuildState) -> BundleFailure {
    let loaded_paths = state.sorted_loaded_paths();
    // NOT `loaded_paths`. That set is recorded at `module_parsed`, so the one module it can never
    // contain is the module that failed to parse — which is precisely the one a cached failure has
    // to expire against. The read-time map is populated in `load`, before Rolldown parses anything,
    // so it has every module whose bytes this build actually read.
    let (read_time_fingerprints, _) = state.read_time_fingerprints();
    if let Some(breach) = state.take_breach() {
        return BundleFailure {
            stage: stage::MODULE_GRAPH_LIMIT.to_owned(),
            message: breach,
            diagnostics: error_diagnostics(&diagnostics),
            loaded_paths,
            read_time_fingerprints,
        };
    }

    let failure_stage = diagnostics
        .iter()
        .map(stage_for)
        .find(|candidate| *candidate != stage::LINK)
        .unwrap_or(stage::LINK);
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
        stage: failure_stage.to_owned(),
        message,
        diagnostics: error_diagnostics(&diagnostics),
        loaded_paths,
        read_time_fingerprints,
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
                stage::AMBIGUOUS_EXPORT
            } else {
                stage::MISSING_EXPORT
            }
        }
        EventKind::AmbiguousExternalNamespaceError => stage::AMBIGUOUS_EXPORT,
        EventKind::ParseError | EventKind::JsonParseError | EventKind::TransformError => {
            stage::PARSE
        }
        EventKind::UnresolvedEntry
        | EventKind::UnresolvedImport
        | EventKind::ResolveError
        | EventKind::UnloadableDependencyError => stage::RESOLVE,
        _ => stage::LINK,
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
            stage: stage::GENERATE.to_owned(),
            message: format!("{}: {}", warning.kind(), warning),
        })
        .collect()
}
