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
use crate::cache::key::sort_and_dedup_fingerprints;
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
        translate(output, &state)
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
        let (read_time_fingerprints, unhashed_paths) = build_observations(&state);
        let mut diagnostics = contract_diagnostics(&output.warnings);
        diagnostics.extend(asset_io_diagnostic(&state));

        Ok(ExportEnumeration {
            names: chunk.exports.iter().map(|name| name.to_string()).collect(),
            // A successful build's warnings used to be dropped on the floor.
            diagnostics,
            read_time_fingerprints,
            loaded_paths: state.sorted_loaded_paths(),
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

fn build_observations(
    state: &BuildState,
) -> (Vec<crate::cache::key::FileFingerprint>, Vec<PathBuf>) {
    let (mut fingerprints, unhashed_paths) = state.read_time_fingerprints();
    fingerprints.extend(state.unverifiable_asset_fingerprints());
    sort_and_dedup_fingerprints(&mut fingerprints);
    (fingerprints, unhashed_paths)
}

fn asset_io_diagnostic(state: &BuildState) -> Option<ImportDiagnostic> {
    let paths = state.failed_asset_paths();
    if paths.is_empty() {
        return None;
    }
    let names = paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join(", ");
    Some(ImportDiagnostic {
        stage: stage::ASSET_IO.to_owned(),
        message: format!(
            "supported asset input(s) could not be read during this analysis; retry after the \
             filesystem settles: {names}"
        ),
    })
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

    // Two sources, one meaning: bytes this build knows ship and cannot count. Rolldown emitting an
    // asset beside the chunk (nothing does today), and a directly imported file outside the
    // measured taxonomy. They share a disclosure because the user's question is the same for both.
    let mut emitted = emitted_assets(&output);
    emitted.extend(state.unmeasured_assets());
    emitted.sort_by(|left, right| left.path.cmp(&right.path));
    let mut diagnostics = contract_diagnostics(&output.warnings);
    diagnostics.extend(asset_io_diagnostic(state));
    diagnostics.extend(uncounted_assets_diagnostic(&emitted));
    for import in &chunk.imports {
        diagnostics.push(ImportDiagnostic {
            stage: diagnostic_stage::EXTERNAL.to_owned(),
            message: format!("external module kept as an import boundary: {import}"),
        });
    }
    // There used to be a `side_effects` diagnostic here, pushed for EVERY glob declaration: "matched
    // paths are unavailable from public bundler metadata, so side-effect confidence is
    // conservative". Its premise was retracted (§10.7) and the daemon now matches the entry with
    // `fast_glob` — Rolldown's own matcher — so the matched paths are not unavailable at all: the
    // one that decides the badge is answered exactly. The diagnostic was the last thing holding
    // every `["**/*.css"]` package below High confidence, and it also made `BundleEntry` carry a
    // `reported_side_effects` nobody else read. Both are gone. Rolldown still owns retention
    // (FR-021); the daemon only reports what the package declared about the entry it measured.

    let (read_time_fingerprints, unhashed_paths) = build_observations(state);

    Ok(BundleArtifact {
        code: chunk.code.clone(),
        graph_source_bytes: state.graph_source_bytes(),
        loaded_paths: state.sorted_loaded_paths(),
        read_time_fingerprints,
        unhashed_paths,
        contributions,
        exported_names: chunk.exports.iter().map(|name| name.to_string()).collect(),
        diagnostics,
        matched_side_effect_paths: Vec::new(),
        assets: state.sorted_assets(),
        emitted_assets: emitted,
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
        let mut diagnostics = contract_diagnostics(&output.warnings);
        let asset_io = asset_io_diagnostic(state);
        diagnostics.extend(asset_io.clone());
        let message = asset_io.map_or_else(
            || {
                format!(
                "expected exactly one JavaScript chunk, got {}; a split graph cannot be measured \
                 from one chunk without under-reporting the rest",
                chunks.len()
                )
            },
            |diagnostic| diagnostic.message,
        );
        return Err(BundleFailure {
            stage: if state.failed_asset_paths().is_empty() {
                stage::OUTPUT_SHAPE.to_owned()
            } else {
                stage::ASSET_IO.to_owned()
            },
            message,
            diagnostics,
            loaded_paths: state.sorted_loaded_paths(),
            read_time_fingerprints: build_observations(state).0,
        });
    }
    Ok(chunks.remove(0))
}

/// Bytes this build knows about that it cannot process, named and totalled.
///
/// The stylesheets, wasm and fonts the graph imported are NOT here: the plugin classifies them and
/// the pipeline processes them the way they ship and counts them (B2). What is left is anything
/// **Rolldown itself emitted** beside the chunk — nothing does today, CSS included, but the
/// output-shape guard no longer treats one as fatal, so it must not be silent either. There is no
/// file on disk behind an emitted asset to run a processor over, so it is disclosed, not counted.
///
/// See [`diagnostic_stage::UNCOUNTED_ASSETS`] for why disclosing bytes costs the result its High
/// confidence rather than being exempted.
fn emitted_assets(output: &rolldown::BundleOutput) -> Vec<UncountedAsset> {
    output
        .assets
        .iter()
        .filter_map(|item| match item {
            Output::Asset(asset) => Some(UncountedAsset {
                path: PathBuf::from(asset.filename.to_string()),
                bytes: asset.source.as_bytes().len() as u64,
            }),
            Output::Chunk(_) => None,
        })
        .collect()
}

fn uncounted_assets_diagnostic(assets: &[UncountedAsset]) -> Option<ImportDiagnostic> {
    if assets.is_empty() {
        return None;
    }

    Some(ImportDiagnostic {
        stage: diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
        message: super::uncounted_assets_message(assets),
    })
}

fn classify_failure(diagnostics: Vec<BuildDiagnostic>, state: &BuildState) -> BundleFailure {
    let loaded_paths = state.sorted_loaded_paths();
    // NOT `loaded_paths`. That set is recorded at `module_parsed`, so the one module it can never
    // contain is the module that failed to parse — which is precisely the one a cached failure has
    // to expire against. The read-time map is populated in `load`, before Rolldown parses anything,
    // so it has every module whose bytes this build actually read.
    let (read_time_fingerprints, _) = build_observations(state);
    // A breach preempts every diagnostic below, and the ranking agrees: `MODULE_GRAPH_LIMIT` is the
    // first deterministic stage in `engine::stage`, because a blown graph limit is a fact about the
    // WHOLE build — it was too big to complete — and not about any one module in it. The two used to
    // disagree: the stage was ranked where the breach is DETECTED (the plugin's `load` hook, after
    // resolve), so the declared order promised `resolve` would win a build this arm has always
    // answered `module_graph_limit`.
    //
    // It is not a redundant fast path. `stage_for` maps Rolldown event kinds, and NONE of them is a
    // graph-limit breach — the limit is ours, enforced in the plugin — so the ranking below can
    // never produce this stage. Remove this arm and a breaching build reports the resolve error of
    // some module inside a graph that was abandoned: the shrapnel, with the reason hidden.
    if let Some(breach) = state.take_breach() {
        return BundleFailure {
            stage: stage::MODULE_GRAPH_LIMIT.to_owned(),
            message: breach,
            diagnostics: contract_diagnostics(&diagnostics),
            loaded_paths,
            read_time_fingerprints,
        };
    }

    // BELOW the breach, deliberately. An unreadable asset input is request-local and says "retry
    // after the filesystem settles"; a blown graph limit is a permanent fact about the package. When
    // both are true the breach is the answer, because reporting the transient one erases a DURABLE
    // stage: `ASSET_IO` is absent from `DURABLE_RESULT_STAGES` while `MODULE_GRAPH_LIMIT` is in it,
    // so the mislabel also refuses the failure from every cache and rebuilds the oversized graph on
    // every keystroke. This arm used to sit above the breach and did exactly that.
    if let Some(asset_io) = asset_io_diagnostic(state) {
        let mut diagnostics = contract_diagnostics(&diagnostics);
        diagnostics.push(asset_io.clone());
        return BundleFailure {
            stage: stage::ASSET_IO.to_owned(),
            message: asset_io.message,
            diagnostics,
            loaded_paths,
            read_time_fingerprints,
        };
    }

    // THE EARLIEST STAGE PRESENT, not the first diagnostic in the vector. Rolldown accumulates
    // these from module tasks it runs concurrently, so their order is a race — and this stage is
    // what the user sees (ADR-0006: a failed build has no size, so the stage is the whole answer)
    // AND what the cache stores. Ranking by pipeline position makes it a fact about the bytes.
    // `engine::stage::rank` is where the order lives, and why.
    let failure_stage = diagnostics
        .iter()
        .map(stage_for)
        .min_by_key(|candidate| stage::rank(candidate))
        .unwrap_or(stage::LINK);
    let (read_time_fingerprints, _) = build_observations(state);
    // Rendered from the SAME ordering, for the same reason: the message and the diagnostic list are
    // durable values too, and a message whose lines are shuffled by task timing is a different
    // cached answer for unchanged bytes.
    let diagnostics = contract_diagnostics(&diagnostics);
    let message = if diagnostics.is_empty() {
        "rolldown build failed without diagnostics".to_owned()
    } else {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };

    BundleFailure {
        stage: failure_stage.to_owned(),
        message,
        diagnostics,
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
///
/// **Errors and warnings go through the very same mapping.** Warnings used to be stamped
/// `generate` wholesale, which mislabelled the one diagnostic a user is most likely to meet: an
/// unresolved import is a **warning** — Rolldown externalizes it and the build SUCCEEDS (construct
/// matrix rows 24/25) — so the note saying "this package imports something that is not installed
/// and its bytes are not in this number" arrived labelled as a code-generation problem, on a
/// perfectly good measurement. A diagnostic's stage is where it came from; which side of the
/// build it landed on does not change that.
///
/// **Sorted, because the input order is a race.** Rolldown accumulates both vectors from module
/// tasks it runs concurrently, and these diagnostics are cached on the result. Ordering them by
/// rank, then by text, makes the stored value a function of the bytes rather than of the machine
/// the build happened to run on.
fn contract_diagnostics(diagnostics: &[BuildDiagnostic]) -> Vec<ImportDiagnostic> {
    let mut contract: Vec<ImportDiagnostic> = diagnostics
        .iter()
        .map(|diagnostic| ImportDiagnostic {
            stage: stage_for(diagnostic).to_owned(),
            message: format!("{}: {}", diagnostic.kind(), diagnostic),
        })
        .collect();
    contract.sort_by(|left, right| {
        stage::rank(&left.stage)
            .cmp(&stage::rank(&right.stage))
            .then_with(|| left.message.cmp(&right.message))
    });
    contract
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::plugin::BuildState;

    /// A missing optional asset says "retry after the filesystem settles"; a blown graph limit is a
    /// permanent fact about the package. When both are recorded, the breach has to win.
    ///
    /// Reporting the transient one instead does two things, and the second is the expensive one:
    /// the user is told to retry a condition that will never change, and — because `asset_io` is
    /// absent from `DURABLE_RESULT_STAGES` while `module_graph_limit` is in it — the failure is
    /// refused by every durable store, so the oversized graph is rebuilt on every keystroke.
    ///
    /// This is a Guard: the asset arm was once inserted ABOVE the breach, contradicting the ordering
    /// comment in this very file. Re-inserting it there turns this red.
    #[test]
    fn a_durable_breach_outranks_a_transient_asset_read_failure() {
        let state = BuildState::default();
        state.record_failed_asset_input(PathBuf::from("/pkg/optional.css"));
        state.record_breach("module graph exceeds the 2000 internal module limit");

        let failure = classify_failure(Vec::new(), &state);

        assert_eq!(
            failure.stage,
            stage::MODULE_GRAPH_LIMIT,
            "a permanent property of the package must not be reported as a transient read: {failure:?}"
        );
        assert!(
            failure.message.contains("2000 internal module limit"),
            "the breach's own message is the answer, not the asset retry text: {failure:?}"
        );
        assert!(
            crate::pipeline::stage::may_enter_a_durable_store(&failure.stage),
            "a deterministic breach must stay cacheable so the graph is not rebuilt every request"
        );
    }

    /// The converse, so the guard above cannot be satisfied by simply never reporting `asset_io`.
    #[test]
    fn a_transient_asset_read_failure_still_wins_when_no_breach_was_recorded() {
        let state = BuildState::default();
        state.record_failed_asset_input(PathBuf::from("/pkg/optional.css"));

        let failure = classify_failure(Vec::new(), &state);

        assert_eq!(failure.stage, stage::ASSET_IO, "{failure:?}");
        assert!(
            !crate::pipeline::stage::may_enter_a_durable_store(&failure.stage),
            "a filesystem moment must not be cached as a package fact"
        );
    }
}
