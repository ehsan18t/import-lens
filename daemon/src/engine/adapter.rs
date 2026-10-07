//! Rolldown adapter (spec §7/§8). This file and `plugin.rs` are the only
//! places allowed to import the `rolldown` crate family; every public
//! surface translates to the contract types in `mod.rs`.

use std::collections::HashSet;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use rolldown::plugin::Pluginable;
use rolldown::{
    AttachDebugInfo, Bundler, BundlerOptions, ChecksOptions, CodeSplittingMode,
    ExperimentalOptions, InputItem, IsExternal, OutputFormat, Platform, PreserveEntrySignatures,
    RawMinifyOptions, ResolveOptions,
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
        let package_root = first_entry.package_root.clone();
        let (output, state, unbound_imports) = build_with_retries(|| {
            let input = InputItem {
                name: None,
                import: entry::VIRTUAL_ENTRY_ID.to_owned(),
            };
            (
                build_options(input, package_root.clone(), request.runtime),
                ImportLensPlugin::for_request(&request),
            )
        })
        .await?;
        translate(output, &state, unbound_imports)
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
        let (output, state, unbound_imports) = build_with_retries(|| {
            let input = InputItem {
                name: None,
                import: rolldown_entry_path(&entry_path),
            };
            (
                build_options(input, cwd.clone(), runtime),
                ImportLensPlugin::passthrough(),
            )
        })
        .await?;
        let chunk = single_chunk(&output, &state)?;
        let (read_time_fingerprints, unhashed_paths) = build_observations(&state);
        let mut diagnostics = contract_diagnostics(&output.warnings);
        diagnostics.extend(unbound_imports);
        diagnostics.extend(asset_io_diagnostic(&state));

        Ok(ExportEnumeration {
            names: chunk.exports.iter().map(|name| name.to_string()).collect(),
            diagnostics,
            read_time_fingerprints,
            loaded_paths: state.sorted_loaded_paths(),
            unhashed_paths,
        })
    }
}

fn rolldown_entry_path(path: &Path) -> String {
    let normalized = crate::cache::key::identity_path_string(path);
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
    fingerprints.extend(state.asset_input_fingerprints());
    sort_and_dedup_fingerprints(&mut fingerprints);
    (fingerprints, unhashed_paths)
}

fn asset_io_diagnostic(state: &BuildState) -> Option<ImportDiagnostic> {
    // Unreadable ONLY. An absent input is a deterministic fact about the package; routing it to
    // the non-durable `asset_io` stage would refuse a correct measurement from every cache.
    let paths = state.unreadable_asset_paths();
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
        external: Some(BUILTIN_EXTERNAL.clone()),
        format: Some(OutputFormat::Esm),
        // Strict signatures keep every requested `__il_entry_*` alias alive
        // verbatim in the chunk's export list.
        preserve_entry_signatures: Some(PreserveEntrySignatures::Strict),
        // Bool(false) inlines dynamic imports into the single chunk; the pinned
        // Rolldown has no separate inline-dynamic-imports option.
        code_splitting: Some(CodeSplittingMode::Bool(false)),
        // None is NOT off: it normalizes to dead-code-elimination minification.
        // The raw chunk must stay byte-faithful (§8.1).
        minify: Some(RawMinifyOptions::Bool(false)),
        // An UNSET platform derives `Browser` from `Esm`, which appends `browser` to
        // our condition list and injects a `process.env.NODE_ENV` define; both
        // corrupt measurement for the Server runtime. `Neutral` leaves the shared
        // resolver's per-runtime condition list authoritative (§7.1).
        platform: Some(Platform::Neutral),
        // An UNSET attach_debug_info normalizes to `Simple`, which wraps every module
        // in `//#region` comments that land in `raw_bytes` and in each module's
        // `rendered_length` (§8.1/§8.2).
        experimental: Some(ExperimentalOptions {
            attach_debug_info: Some(AttachDebugInfo::None),
            ..ExperimentalOptions::default()
        }),
        resolve: Some(resolve_options_for(runtime)),
        // Advisories about the build, not about the bytes, and every warning that reaches the
        // result costs it its High confidence. A module-level directive (`"use client"`) is dropped
        // as in any bundler; the other two are build-speed advice.
        checks: Some(ChecksOptions {
            module_level_directive: Some(false),
            large_barrel_modules: Some(false),
            bundler_timings: Some(false),
            ..ChecksOptions::default()
        }),
        ..BundlerOptions::default()
    }
}

/// Node builtins stay external (§7.1), matched by exact string equality, the same test
/// Rolldown applies to a string list. Rolldown asks this about every specifier and every
/// resolved id (about 4,600 calls for one lodash-es build), so the answer must not allocate:
/// it is a zero-sized future, and a length check rejects paths before the set lookup.
static BUILTIN_EXTERNAL: LazyLock<IsExternal> = LazyLock::new(|| {
    let mut specifiers =
        HashSet::with_capacity(NODE_BUILTIN_MODULES.len() * 2 + NODE_PREFIX_ONLY_MODULES.len());
    for module in NODE_BUILTIN_MODULES {
        specifiers.insert((*module).to_owned());
        specifiers.insert(format!("node:{module}"));
    }
    // Prefix-only builtins carry their `node:` prefix already, and must NOT be added
    // bare: the bare spelling belongs to whatever npm package owns that name.
    for module in NODE_PREFIX_ONLY_MODULES {
        specifiers.insert((*module).to_owned());
    }
    let longest = specifiers.iter().map(String::len).max().unwrap_or(0);
    IsExternal::Fn(Some(Arc::new(
        move |specifier: &str, _importer, _is_resolved| {
            if specifier.len() <= longest && specifiers.contains(specifier) {
                Box::pin(Answer::<_, true>(PhantomData))
            } else {
                Box::pin(Answer::<_, false>(PhantomData))
            }
        },
    )))
});

/// A ready `Ok(EXTERNAL)` that is zero-sized, so boxing it does not allocate.
struct Answer<E, const EXTERNAL: bool>(PhantomData<fn() -> E>);

impl<E, const EXTERNAL: bool> Future for Answer<E, EXTERNAL> {
    type Output = Result<bool, E>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(EXTERNAL))
    }
}

/// The mapped resolve options per runtime, built once; each build clones its runtime's.
fn resolve_options_for(runtime: ImportRuntime) -> ResolveOptions {
    static MAPPED: LazyLock<[ResolveOptions; 3]> = LazyLock::new(|| {
        [
            ImportRuntime::Component,
            ImportRuntime::Client,
            ImportRuntime::Server,
        ]
        .map(mapped_resolve_options)
    });
    let [component, client, server] = &*MAPPED;
    match runtime {
        ImportRuntime::Component => component.clone(),
        ImportRuntime::Client => client.clone(),
        ImportRuntime::Server => server.clone(),
    }
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
    let mut bundler = Bundler::with_plugins(options, vec![Pluginable::new_shared(plugin)])
        .map_err(|error| classify_failure(error.into_vec(), state))?;
    let result = bundler.generate().await;
    // Release plugin-driver resources even when the build failed.
    let _ = bundler.close().await;
    result.map_err(|error| classify_failure(error.into_vec(), state))
}

/// Run one build, and retry it at most twice, each retry switching on one thing for good:
///
/// * a graph-limit breach retries with Rolldown's lazy barrel, which leaves unloaded every module a
///   side-effect-free barrel re-exports and the import never reaches. Rolldown otherwise loads the
///   whole barrel before tree-shaking it, so `import { Home } from "@mui/icons-material"` breaches
///   while measuring one icon. Only a breach turns it on: it changes the bytes of builds that
///   already succeed, so turning it on everywhere would move numbers that are right today;
/// * when [`is_internal_unbound_import`] admits the failure, the build runs with the unmatched
///   bindings stubbed (`shim_missing_exports`).
///
/// Returns the output, the state of the attempt that produced it, and the unbound-import attempt's
/// diagnostics: a stubbed build reports nothing of its own, so those are the whole disclosure.
///
/// Every attempt gets a fresh plugin and state: a failed one recorded the paths and fingerprints
/// of a graph that was thrown away, and freshness must describe the graph the answer came from.
async fn build_with_retries(
    attempt: impl Fn() -> (BundlerOptions, ImportLensPlugin),
) -> Result<
    (
        rolldown::BundleOutput,
        Arc<BuildState>,
        Vec<ImportDiagnostic>,
    ),
    BundleFailure,
> {
    let mut lazy_barrel = false;
    let mut unbound_imports: Option<Vec<ImportDiagnostic>> = None;
    loop {
        let (mut options, plugin) = attempt();
        if lazy_barrel && let Some(experimental) = options.experimental.as_mut() {
            experimental.lazy_barrel = Some(true);
        }
        if unbound_imports.is_some() {
            options.shim_missing_exports = Some(true);
        }
        let state = plugin.state();
        match run_build(options, plugin, &state).await {
            Ok(output) => return Ok((output, state, unbound_imports.unwrap_or_default())),
            Err(failure)
                if !lazy_barrel
                    && unbound_imports.is_none()
                    && failure.stage == stage::MODULE_GRAPH_LIMIT =>
            {
                lazy_barrel = true;
            }
            Err(failure)
                if unbound_imports.is_none()
                    && is_internal_unbound_import(&failure, &state.entry_stable_ids()) =>
            {
                unbound_imports = Some(failure.diagnostics);
            }
            Err(failure) => return Err(failure),
        }
    }
}

/// Whether a failed build may be retried with the unmatched binding stubbed.
///
/// Rolldown raises an unmatched import at `Severity::Error`, so one broken edge **anywhere** in a
/// package's graph would leave the whole package unmeasured.
///
/// Stubbing is sound for an edge BETWEEN dependencies: every module still renders at its true
/// bytes. It is refused when the build's ENTRY is the importer. For a size build that is the
/// virtual entry, which imports exactly the export the user requested; guessing it is what the
/// SRS forbids and would turn a typo into a confident size. For export enumeration it is the real
/// entry, whose export surface is the answer, so a stub would offer a name that does not exist.
///
/// `ambiguous_export` is excluded: `shim_missing_exports` only rewrites a `NoMatch` binding, so
/// the retry would fail identically.
fn is_internal_unbound_import(failure: &BundleFailure, entry_stable_ids: &[String]) -> bool {
    // No recorded entry means nothing can be protected, so nothing may be stubbed.
    !entry_stable_ids.is_empty()
        && !failure.diagnostics.is_empty()
        && failure.diagnostics.iter().all(|diagnostic| {
            diagnostic.stage == stage::MISSING_EXPORT
                && !entry_stable_ids.iter().any(|entry| {
                    diagnostic
                        .message
                        .contains(&format!("imported by \"{entry}\""))
                })
        })
}

fn translate(
    output: rolldown::BundleOutput,
    state: &BuildState,
    unbound_imports: Vec<ImportDiagnostic>,
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

    // Two sources, one disclosure: bytes this build knows ship and cannot count. An asset Rolldown
    // emitted beside the chunk, and a directly imported file outside the measured taxonomy.
    let mut emitted = emitted_assets(&output);
    emitted.extend(state.unmeasured_assets());
    emitted.sort_by(|left, right| left.path.cmp(&right.path));
    let mut diagnostics = contract_diagnostics(&output.warnings);
    // Carried from the attempt that FAILED, because a stubbed build emits no diagnostic. They are
    // the whole disclosure, and what holds the result at Medium rather than High confidence.
    if !unbound_imports.is_empty() {
        diagnostics.extend(unbound_imports);
        diagnostics.push(ImportDiagnostic {
            stage: stage::MISSING_EXPORT.to_owned(),
            // A FLOOR, and it must say so: a real binding would retain whatever implements it, so
            // a stub measures the graph as installed, not the graph a working version would have.
            message: "a dependency imports a binding its source module does not export; the graph \
                      was measured with that binding stubbed, so whatever the real binding would \
                      have retained is NOT in this size, and the import named above is `undefined` \
                      wherever it is used"
                .to_owned(),
        });
    }
    diagnostics.extend(asset_io_diagnostic(state));
    diagnostics.extend(uncounted_assets_diagnostic(&emitted));
    for import in &chunk.imports {
        diagnostics.push(ImportDiagnostic {
            stage: diagnostic_stage::EXTERNAL.to_owned(),
            message: format!("external module kept as an import boundary: {import}"),
        });
    }
    // A boundary the package did not ask for. Rolldown externalizes an unresolvable bare specifier
    // it answers `NotFound` (reported at `resolve` from its warning), and the plugin extends that to
    // the denial variants, reported here at the same stage. The graph behind such an edge is absent
    // from the number, so the result is a floor.
    for specifier in state.unresolved_externals() {
        diagnostics.push(ImportDiagnostic {
            stage: stage::RESOLVE.to_owned(),
            message: format!(
                "could not resolve '{specifier}'; kept as an import boundary, so anything it would \
                 have pulled in is NOT in this size"
            ),
        });
    }
    let (read_time_fingerprints, unhashed_paths) = build_observations(state);
    let exported_names = chunk.exports.iter().map(|name| name.to_string()).collect();

    // The output holds the chunk's other reference; once it is gone the code moves out
    // instead of being copied.
    drop(output);
    let code = Arc::try_unwrap(chunk).map_or_else(|shared| shared.code.clone(), |owned| owned.code);

    Ok(BundleArtifact {
        code,
        graph_source_bytes: state.graph_source_bytes(),
        loaded_paths: state.sorted_loaded_paths(),
        read_time_fingerprints,
        unhashed_paths,
        contributions,
        exported_names,
        diagnostics,
        assets: state.sorted_assets(),
        emitted_assets: emitted,
    })
}

/// The build must produce exactly one JavaScript **chunk** (§7.1): a size taken from one of
/// several code-split chunks would under-report the package, so that is a typed `output_shape`
/// failure.
///
/// An emitted **asset** is not: it makes the chunk incomplete, not wrong, and
/// [`uncounted_assets_diagnostic`] discloses it. Rolldown emits no asset for a stylesheet (the
/// plugin stubs CSS, FR-018a). "One chunk" is the invariant; "no assets" is not.
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
            stage: if state.unreadable_asset_paths().is_empty() {
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
/// the pipeline counts them. This is anything **Rolldown itself emitted** beside the chunk; there
/// is no file on disk behind it to process, so it is disclosed, not counted.
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
        message: super::uncounted_assets_message(assets, false),
    })
}

fn classify_failure(diagnostics: Vec<BuildDiagnostic>, state: &BuildState) -> BundleFailure {
    let loaded_paths = state.sorted_loaded_paths();
    // NOT `loaded_paths`: that set is recorded at `module_parsed`, so it never contains the module
    // that failed to parse, which is the one a cached failure must expire against. The read-time
    // map is populated in `load`, before parsing.
    let (read_time_fingerprints, _) = build_observations(state);
    // A breach preempts every diagnostic below, matching `engine::stage`, where
    // `MODULE_GRAPH_LIMIT` is the first deterministic stage: it is a fact about the WHOLE build.
    //
    // Not a redundant fast path: no Rolldown event kind is a graph-limit breach (the limit is
    // enforced in the plugin), so the ranking below can never produce this stage. Without this arm
    // a breaching build reports some resolve error from the abandoned graph.
    if let Some(breach) = state.take_breach() {
        return BundleFailure {
            stage: stage::MODULE_GRAPH_LIMIT.to_owned(),
            message: breach,
            diagnostics: contract_diagnostics(&diagnostics),
            loaded_paths,
            read_time_fingerprints,
        };
    }

    // BELOW the breach, deliberately. An unreadable asset input is request-local; a blown graph
    // limit is permanent. `ASSET_IO` is absent from `DURABLE_RESULT_STAGES` while
    // `MODULE_GRAPH_LIMIT` is in it, so reporting the transient one would refuse the failure from
    // every cache and rebuild the oversized graph on every keystroke.
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

    // THE EARLIEST STAGE PRESENT, not the first diagnostic in the vector: Rolldown accumulates
    // these from concurrent module tasks, so their order is a race, and this stage is both what the
    // user sees (ADR-0006) and what the cache stores. `engine::stage::rank` holds the order.
    let failure_stage = diagnostics
        .iter()
        .map(stage_for)
        .min_by_key(|candidate| stage::rank(candidate))
        .unwrap_or(stage::LINK);
    let (read_time_fingerprints, _) = build_observations(state);
    // Rendered from the SAME ordering: the message and diagnostic list are cached too, and must not
    // depend on task timing.
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
        EventKind::MissingExportError => stage::MISSING_EXPORT,
        // The pinned Rolldown's only producer of a name claimed by conflicting star providers.
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
/// **Errors and warnings go through the same mapping.** A diagnostic's stage is where it came
/// from, not which side of the build it landed on: an unresolved import is a warning (Rolldown
/// externalizes it and the build succeeds) and must still read as `resolve`.
///
/// **Sorted, because the input order is a race.** Rolldown accumulates both vectors from
/// concurrent module tasks, and these diagnostics are cached; ordering by rank, then text, makes
/// the stored value a function of the bytes.
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
    use crate::engine::plugin::{AssetInputFailure, BuildState};

    fn is_builtin_external(specifier: &str) -> bool {
        use futures_util::FutureExt;
        BUILTIN_EXTERNAL
            .call(specifier, None, false)
            .now_or_never()
            .expect("the builtin answer is ready at once")
            .expect("the builtin answer cannot fail")
    }

    /// Every builtin stays external under both spellings, a prefix-only builtin only under its
    /// `node:` spelling (the bare name belongs to an npm package), and nothing else is external.
    #[test]
    fn exactly_the_node_builtins_are_external() {
        for module in NODE_BUILTIN_MODULES {
            assert!(is_builtin_external(module), "{module}");
            assert!(
                is_builtin_external(&format!("node:{module}")),
                "node:{module}"
            );
        }
        for module in NODE_PREFIX_ONLY_MODULES {
            assert!(is_builtin_external(module), "{module}");
            let bare = module
                .strip_prefix("node:")
                .expect("prefix-only builtins carry node:");
            assert!(!is_builtin_external(bare), "{bare} is an npm package name");
        }
        for specifier in [
            "react",
            "./fs",
            "fs/",
            "node:react",
            "C:/pkg/node_modules/fs/index.js",
        ] {
            assert!(!is_builtin_external(specifier), "{specifier}");
        }
    }

    /// The gate separating "a dependency's broken edge is measured and disclosed" from "a typo in
    /// the user's own import comes back as a confident size". Loosening any arm (the entry check,
    /// admitting `ambiguous_export`, accepting a mixed diagnostic list) turns it red.
    #[test]
    fn only_an_internal_missing_export_may_be_retried_with_a_stub() {
        let failure = |diagnostics: Vec<ImportDiagnostic>| BundleFailure {
            stage: stage::MISSING_EXPORT.to_owned(),
            message: String::new(),
            diagnostics,
            loaded_paths: Vec::new(),
            read_time_fingerprints: Vec::new(),
        };
        let diagnostic = |stage: &str, message: &str| ImportDiagnostic {
            stage: stage.to_owned(),
            message: message.to_owned(),
        };
        let entries = [entry::VIRTUAL_ENTRY_ID.to_owned()];
        let internal_edge = || {
            failure(vec![diagnostic(
                stage::MISSING_EXPORT,
                r#""walk" is not exported by "yuku-parser/index.js", imported by "dts/index.mjs""#,
            )])
        };

        assert!(
            is_internal_unbound_import(&internal_edge(), &entries),
            "an edge between two dependencies is the case this exists for"
        );
        assert!(
            !is_internal_unbound_import(
                &failure(vec![diagnostic(
                    stage::MISSING_EXPORT,
                    &format!(
                        r#""nope" is not exported by "lib/index.js", imported by "{}""#,
                        entry::VIRTUAL_ENTRY_ID
                    ),
                )]),
                &entries
            ),
            "the export the USER requested is the one the virtual entry imports, and guessing that \
             binding is what the SRS forbids"
        );
        assert!(
            !is_internal_unbound_import(&internal_edge(), &["dts/index.mjs".to_owned()]),
            "an enumeration's own entry is protected the same way: a stub there invents an export"
        );
        assert!(
            !is_internal_unbound_import(&internal_edge(), &[]),
            "a build that recorded no entry has nothing it could protect"
        );
        assert!(
            !is_internal_unbound_import(
                &failure(vec![diagnostic(
                    stage::AMBIGUOUS_EXPORT,
                    "name is ambiguous"
                )]),
                &entries
            ),
            "a stub only rewrites a NoMatch binding, so the retry would fail identically"
        );
        assert!(
            !is_internal_unbound_import(
                &failure(vec![
                    diagnostic(stage::MISSING_EXPORT, "an internal edge"),
                    diagnostic(stage::PARSE, "a syntax error"),
                ]),
                &entries
            ),
            "a graph that also fails to parse is not made measurable by stubbing a binding"
        );
        assert!(
            !is_internal_unbound_import(&failure(Vec::new()), &entries),
            "a failure with no diagnostics says nothing about what a retry would do"
        );
    }

    /// An unreadable asset input is transient; a blown graph limit is a permanent fact about the
    /// package. When both are recorded the breach must win, or the failure is refused by every
    /// durable store and the oversized graph is rebuilt on every keystroke. Moving the asset arm
    /// above the breach turns this red.
    #[test]
    fn a_durable_breach_outranks_a_transient_asset_read_failure() {
        let state = BuildState::default();
        state.record_failed_asset_input(
            PathBuf::from("/pkg/optional.css"),
            AssetInputFailure::Unreadable,
        );
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
        state.record_failed_asset_input(
            PathBuf::from("/pkg/optional.css"),
            AssetInputFailure::Unreadable,
        );

        let failure = classify_failure(Vec::new(), &state);

        assert_eq!(failure.stage, stage::ASSET_IO, "{failure:?}");
        assert!(
            !crate::pipeline::stage::may_enter_a_durable_store(&failure.stage),
            "a filesystem moment must not be cached as a package fact"
        );
    }
}
