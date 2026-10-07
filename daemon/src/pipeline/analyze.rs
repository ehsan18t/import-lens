use crate::{
    engine::{dependency_paths::record_loaded_paths, limits::MAX_MODULE_SOURCE_BYTES},
    ipc::protocol::{
        ConfidenceLevel, ImportDiagnostic, ImportKind, ImportRequest, ImportResult, MeasuredSizes,
        ModuleContribution,
    },
    pipeline::{
        assets::{AssetProcessingFailure, asset_diagnostics, process_assets_bounded},
        compress::compress_all,
        fallback::source_excerpt_detail,
        full_package,
        minify::minify_source,
        native_binary::{annotate_native_binary, native_binary_only_package_result},
        resolver::{ResolvedPackage, SideEffectsMode, resolve_package_entry},
        types_only::declaration_only_package_result,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Where an analysis runs, and nothing else.
///
/// It carries no deadline: a request answers from cache and each build is pushed to the client as
/// it lands (`ipc::server`), so no request needs to abandon a build.
#[derive(Debug, Clone)]
pub struct AnalysisContext {
    pub workspace_root: PathBuf,
    pub active_document_path: PathBuf,
}

// Internal structured error translated into the stable ImportResult surface.
#[derive(Debug, Clone, Default)]
struct FailureFreshness {
    /// Fingerprints of every module the failing build read.
    read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    /// Modules parsed before failure, used to find first-party manifests that shaped resolution.
    loaded_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AnalysisError {
    stage: &'static str,
    message: String,
    details: Vec<String>,
    /// Fingerprints of every module the failing build read; empty for a failure that never
    /// entered the engine.
    ///
    /// A deterministic failure is cached (ADR-0006, invariant 3), so it is fingerprinted against
    /// the bytes it was derived from, exactly as a success is. The entry and manifest alone are
    /// not enough: an entry that re-exports the module that fails to parse would keep serving the
    /// failure after the user fixed that module.
    freshness: Box<FailureFreshness>,
}

pub fn analyze_import(context: &AnalysisContext, request: &ImportRequest) -> ImportResult {
    match analyze_import_inner(context, request) {
        Ok(result) => result,
        Err(error) => error_result(request, error),
    }
}

/// Manifests of the first-party packages whose sources this build loaded (§8.3).
///
/// The plugin records graph modules, and a `package.json` is never one, yet it drives resolution
/// and side-effect classification: editing a workspace dependency's `exports`, `type` or
/// `sideEffects` changes what the bundler pulls in while no fingerprinted source moves.
///
/// Installed packages are excluded: their manifests change only with an install, which bumps the
/// cache generation.
pub(super) fn first_party_manifests(
    context: &AnalysisContext,
    loaded_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Loaded paths are canonicalized (verbatim `\\?\C:\...` on Windows) while the workspace
    // root arrives as the editor spelled it; without canonicalizing the root, the walk below
    // would never stop at it.
    let workspace_root = fs::canonicalize(&context.workspace_root)
        .unwrap_or_else(|_| context.workspace_root.clone());

    for path in loaded_paths {
        if path
            .components()
            .any(|component| component.as_os_str() == "node_modules")
        {
            continue;
        }

        // Nearest manifest at or above the module, bounded by the workspace root so a
        // loose file outside the workspace cannot walk to the filesystem root.
        let mut directory = path.parent();
        while let Some(current) = directory {
            if seen.contains(current) {
                break;
            }
            seen.insert(current.to_path_buf());

            let manifest = current.join("package.json");
            if manifest.is_file() {
                manifests.push(manifest);
                break;
            }
            if current == workspace_root {
                break;
            }
            directory = current.parent();
        }
    }

    manifests
}

/// The manifests that decided what a build resolved: the measured package's own, plus the
/// first-party manifests its loaded sources sit under. One helper for the success and failure
/// paths, so a cached failure expires on exactly the manifest edits a cached size does.
fn manifest_stat_paths(
    context: &AnalysisContext,
    package_root: &Path,
    loaded_paths: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = vec![package_root.join("package.json")];
    paths.extend(first_party_manifests(context, loaded_paths));
    paths
}

/// Everything the full-package memo must expire against: the comparison build's read-time
/// fingerprints plus the manifests that decided what it resolved. Must match the import cache's
/// freshness set, or the memo outlives the size it describes.
fn full_package_fingerprints(
    context: &AnalysisContext,
    package_root: &Path,
    full: &crate::engine::BundleArtifact,
) -> Vec<crate::cache::key::FileFingerprint> {
    manifest_augmented_fingerprints(
        context,
        package_root,
        &full.read_time_fingerprints,
        &full.loaded_paths,
    )
}

/// A build's read-time fingerprints plus the package's own manifest and the first-party
/// manifests its sources loaded (§8.3), so a manifest edit no source file reflects still expires
/// a build-derived memo.
///
/// Shared by the full-package and export-list memos so they agree on "still fresh".
pub(crate) fn manifest_augmented_fingerprints(
    context: &AnalysisContext,
    package_root: &Path,
    read_time_fingerprints: &[crate::cache::key::FileFingerprint],
    loaded_paths: &[PathBuf],
) -> Vec<crate::cache::key::FileFingerprint> {
    use crate::cache::key::file_fingerprint_reading_hash;

    let mut fingerprints = read_time_fingerprints.to_vec();
    fingerprints.extend(
        manifest_stat_paths(context, package_root, loaded_paths)
            .into_iter()
            .filter_map(file_fingerprint_reading_hash),
    );
    fingerprints
}

/// Freshness inputs for a successful engine build (§8.3).
///
/// `fingerprints` were captured as each module's bytes were read during the build, so they
/// describe exactly the measured bytes. `stat_paths` holds resolution inputs (manifests) with no
/// read-time capture, for the caller to hash.
pub enum FingerprintSource {
    ReadTime {
        fingerprints: Vec<crate::cache::key::FileFingerprint>,
        stat_paths: Vec<PathBuf>,
    },
}

/// Analyze a pre-resolved entry and return the exact real paths Rolldown loaded.
pub fn analyze_resolved_import_with_dependencies(
    context: &AnalysisContext,
    request: &ImportRequest,
    resolved: ResolvedPackage,
) -> (ImportResult, Option<FingerprintSource>) {
    let package_root = resolved.package_root.clone();
    match analyze_import_inner_resolved(context, request, resolved) {
        Ok((result, source)) => (result, source),
        Err(error) => {
            // A deterministic failure is cached (ADR-0006), so it is fingerprinted against what
            // the engine had loaded when it gave up, not just the entry.
            let source = engine_failure_fingerprints(context, &package_root, &error);
            (error_result(request, error), source)
        }
    }
}

/// Freshness inputs for a failed engine build: the bytes it read plus the manifests that decided
/// what it resolved, the same set the success path (`analyze_with_rolldown_engine`) uses.
///
/// `None` for a failure that never reached the engine (unreadable manifest, unresolvable or
/// oversized entry). `service::dependency_fingerprints` then falls back to the entry and the
/// manifest, which is exactly what would have to change for those answers to change.
fn engine_failure_fingerprints(
    context: &AnalysisContext,
    package_root: &Path,
    error: &AnalysisError,
) -> Option<FingerprintSource> {
    if error.freshness.read_time_fingerprints.is_empty() {
        return None;
    }

    let stat_paths = manifest_stat_paths(context, package_root, &error.freshness.loaded_paths);

    Some(FingerprintSource::ReadTime {
        fingerprints: error.freshness.read_time_fingerprints.clone(),
        stat_paths,
    })
}

fn analyze_import_inner(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ImportResult, AnalysisError> {
    // No arm here invents a size: an unreadable manifest is Unmeasured (ADR-0006).
    let resolved = match resolve_package_entry(&context.active_document_path, request) {
        Ok(resolved) => resolved,
        Err(message) => return unresolved_import_result(context, request, message),
    };
    let (result, _graph) = analyze_import_inner_resolved(context, request, resolved)?;
    Ok(result)
}

/// The answer for an import whose package entry did not resolve, from the resolver's own
/// message. It never resolves again, so it can never start an engine build: callers settle it
/// at pool width instead of queueing it for an engine permit.
pub fn analyze_unresolved_import(
    context: &AnalysisContext,
    request: &ImportRequest,
    resolver_message: String,
) -> ImportResult {
    unresolved_import_result(context, request, resolver_message)
        .unwrap_or_else(|error| error_result(request, error))
}

fn unresolved_import_result(
    context: &AnalysisContext,
    request: &ImportRequest,
    message: String,
) -> Result<ImportResult, AnalysisError> {
    let stage = if message.contains("unsafe package name") {
        crate::pipeline::stage::PACKAGE_VALIDATION
    } else if message.contains("package manifest not found") {
        crate::pipeline::stage::PACKAGE_RESOLUTION
    } else if is_manifest_fallback_error(&message) {
        crate::pipeline::stage::PACKAGE_MANIFEST
    } else if message.contains(crate::pipeline::resolver::NO_ROOT_ENTRY_PHRASE) {
        crate::pipeline::stage::NO_ROOT_ENTRY
    } else {
        crate::pipeline::stage::ENTRY_RESOLUTION
    };

    if stage == crate::pipeline::stage::ENTRY_RESOLUTION
        || stage == crate::pipeline::stage::NO_ROOT_ENTRY
    {
        // A declarations-only package is Measured: it ships zero runtime bytes. Its diagnostic
        // stage keeps `Some(0)` unambiguous.
        if let Some(result) =
            declaration_only_package_result(&context.active_document_path, request)
        {
            return Ok(result);
        }

        // A native-binary-only package (a `bin` plus a platform-specific native binary as
        // `optionalDependencies`, no importable JS entry) is likewise Measured at zero and
        // labelled.
        if let Some(result) =
            native_binary_only_package_result(&context.active_document_path, request)
        {
            return Ok(result);
        }
    }

    let details = resolver_details(&message);
    Err(error_with_context(
        stage, message, context, request, details,
    ))
}

fn is_manifest_fallback_error(message: &str) -> bool {
    message.contains("failed to read package manifest")
        || message.contains("failed to parse package manifest")
        || message.contains("missing a string version")
}

fn analyze_import_inner_resolved(
    context: &AnalysisContext,
    request: &ImportRequest,
    resolved: ResolvedPackage,
) -> Result<(ImportResult, Option<FingerprintSource>), AnalysisError> {
    let side_effects_mode = resolved.side_effects;
    let entry_path = resolved.entry_path;
    let package_root = resolved.package_root;
    let is_cjs = resolved.is_cjs;
    let package_json = resolved.package_json;

    // A stat failure is an IO condition, not a fact about the package, so `entry_metadata` is
    // not durable (see `pipeline::stage`).
    let metadata = fs::metadata(&entry_path).map_err(|error| {
        error_with_context(
            crate::pipeline::stage::ENTRY_METADATA,
            format!(
                "failed to stat package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;

    // An entry over the module source limit is a deterministic property of the package the engine
    // cannot answer, so it is Unmeasured: `oversized_entry` is not in `stage::ALL`, hence not
    // transient, hence cached like any other fact about the code.
    if metadata.len() as usize > MAX_MODULE_SOURCE_BYTES {
        return Err(error_with_context(
            crate::pipeline::stage::OVERSIZED_ENTRY,
            format!(
                "entry file exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit, so no module graph could be built"
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        ));
    }

    // A failed engine build is Unmeasured, never sized from the entry file alone.
    let (mut result, loaded_paths, freshness) = analyze_with_rolldown_engine(
        context,
        request,
        &entry_path,
        &package_root,
        &side_effects_mode,
        is_cjs,
    )?;
    // A package whose JS entry resolved but which is backed by a platform-specific native binary
    // keeps its measured JS size and carries a `native_binary` flag beside it, so a thin shim (the
    // TypeScript 7 version stub) is not read as the whole cost.
    annotate_native_binary(&mut result, &package_json);
    record_loaded_paths(entry_path, request.runtime, loaded_paths);
    Ok((result, Some(freshness)))
}

/// Rolldown-backed analysis (§8): one engine build produces the raw chunk, OXC minifies it, and
/// the compression pipeline runs over the minified string. Returns the loaded real paths and the
/// §8.3 freshness inputs alongside the result.
pub(crate) fn analyze_with_rolldown_engine(
    context: &AnalysisContext,
    request: &ImportRequest,
    entry_path: &Path,
    package_root: &Path,
    side_effects_mode: &SideEffectsMode,
    is_cjs: bool,
) -> Result<(ImportResult, Vec<PathBuf>, FingerprintSource), AnalysisError> {
    use crate::engine::{BundleEntry, BundlePurpose, BundleRequest, BundleSelection, boundary};

    let bundle_entry = |selection: BundleSelection| BundleEntry {
        entry_path: entry_path.to_path_buf(),
        package_root: package_root.to_path_buf(),
        selection,
    };
    let artifact = boundary::bundle_sync(BundleRequest {
        entries: vec![bundle_entry(engine_selection(request))],
        runtime: request.runtime,
        purpose: BundlePurpose::ImportSize,
    })
    .map_err(|failure| engine_error(context, request, failure))?;

    let minified = minify_source(&artifact.code).map_err(|error| {
        error_with_context(
            crate::pipeline::stage::MINIFY,
            format!("failed to minify engine chunk: {error}"),
            context,
            request,
            vec![source_excerpt_detail(&artifact.code)],
        )
    })?;
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            crate::pipeline::stage::COMPRESSION,
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;

    // The package's non-JavaScript assets, processed the way they ship, join the Import Cost.
    // Each artifact is compressed on its own and summed (ADR-0005). Parse/compression failures and
    // a resource-ledger breach disclose raw bytes beside the measured JavaScript; only a
    // request-local stage failure (deadline, panic, lost runtime) leaves the import Unmeasured.
    let assets = process_assets_bounded(
        artifact.assets.clone(),
        artifact.graph_source_bytes,
        artifact.loaded_paths.clone(),
    )
    .map_err(|failure| asset_processing_error(context, request, &artifact, failure))?;
    let asset_sizes = assets.total();

    // §7.4/FR-021: Side-Effectful is a property of the import (does the package declare the
    // measured entry effectful?), so the glob form answers by matching the entry and
    // `has_side_effects` is the whole answer. Do not OR in `is_array()`: `["**/*.css"]` says
    // nothing about a JavaScript entry, and it would gate off the comparison below.
    let side_effects = side_effects_mode.has_side_effects();
    let mut diagnostics: Vec<ImportDiagnostic> = artifact
        .diagnostics
        .iter()
        .map(|diagnostic| ImportDiagnostic {
            stage: diagnostic.stage.clone(),
            message: diagnostic.message.clone(),
            details: Vec::new(),
        })
        .collect();
    // An asset that could not be processed is disclosed: its bytes ship but are not in the number.
    diagnostics.extend(asset_diagnostics(&assets));

    // Full-package comparison (§8.4/§6.3): a second engine build measures the complete surface;
    // failure degrades to "not treeshakeable", never an analysis error. Memoized per entry
    // because the answer does not depend on which names were imported.
    let mut truly_treeshakeable = false;
    if !side_effects
        && matches!(request.import_kind, ImportKind::Named)
        && !request.named.is_empty()
    {
        let full_len = full_package::lookup(entry_path, request.runtime).or_else(|| {
            // Read before the build: an invalidation landing mid-build must not be stamped onto
            // a length measured from the bytes it invalidated.
            let generation = crate::cache::memory::cache_generation();
            let full = match boundary::bundle_sync(BundleRequest {
                entries: vec![bundle_entry(BundleSelection::Full)],
                runtime: request.runtime,
                purpose: BundlePurpose::FullPackageComparison,
            }) {
                Ok(full) => full,
                Err(failure) => {
                    // Reported under the stage the build failed at, not an invented label (§12).
                    // That lets `should_cache_result` see a transient failure: caching
                    // `truly_treeshakeable: false` after a timeout would mark a healthy package
                    // "not tree-shakeable" for a whole cache generation.
                    diagnostics.push(ImportDiagnostic {
                        stage: contract_stage(&failure.stage).to_owned(),
                        message: format!(
                            "full-package comparison build failed; treating as not tree-shakeable: {}",
                            failure.message
                        ),
                        details: Vec::new(),
                    });
                    return None;
                }
            };

            let full_len = minify_source(&full.code).ok()?.len() as u64;
            // A graph with a module the plugin could not fingerprint (a binary module) has no
            // complete read-time record to expire a memo against: use it, store nothing.
            if full.unhashed_paths.is_empty() {
                full_package::store(
                    entry_path,
                    request.runtime,
                    full_len,
                    full_package_fingerprints(context, package_root, &full),
                    generation,
                );
            }
            Some(full_len)
        });

        if let Some(full_len) = full_len
            && full_len > 0
        {
            // Within 5% of the full size is not truly tree-shakeable.
            let ratio = (minified.len() as f64) / (full_len as f64);
            truly_treeshakeable = ratio <= 0.95;
        }
    }

    let (confidence, confidence_reasons) = engine_confidence(side_effects, &diagnostics);
    let mut contributions: Vec<ModuleContribution> = artifact
        .contributions
        .iter()
        .map(|contribution| ModuleContribution {
            path: contribution.path.to_string_lossy().to_string(),
            bytes: contribution.rendered_bytes as u64,
        })
        .collect();
    // Assets are contributors too. A stylesheet links as an empty module (rendered length 0), so
    // without these rows the breakdown would not reconcile with the headline, and
    // `annotate_shared_bytes` (which unions on runtime and contribution path) could not see a
    // sheet two imports share. The rows reach `internal_contributions`, which the L2 envelope
    // carries, so a cached result shares identically to a fresh one.
    //
    // Merged by path, not appended: a stylesheet that is the import's own entry already has a row
    // carrying its wrapper bytes. The asset row wins because it is the shipped size.
    for asset in &artifact.assets {
        let path = asset.path.to_string_lossy().to_string();
        match contributions
            .iter_mut()
            .find(|contribution| contribution.path == path)
        {
            Some(existing) => existing.bytes = asset.raw_bytes(),
            None => contributions.push(ModuleContribution {
                path,
                bytes: asset.raw_bytes(),
            }),
        }
    }

    // §8.3: freshness comes from fingerprints captured by the same reads that supplied every
    // measured byte. The plugin owns JavaScript and directly imported asset snapshots; the asset
    // processor owns CSS `@import` children and local resources discovered through `url()`.
    let freshness = import_freshness(
        artifact.read_time_fingerprints.clone(),
        &artifact.unhashed_paths,
        assets.freshness_fingerprints(),
        manifest_stat_paths(context, package_root, &artifact.loaded_paths),
    );
    let mut loaded_paths = artifact.loaded_paths;
    loaded_paths.extend(assets.read_paths.iter().cloned());
    loaded_paths.sort();
    loaded_paths.dedup();

    let mut result = ImportResult::measured(
        request.specifier.clone(),
        MeasuredSizes {
            raw_bytes: artifact.code.len() as u64 + asset_sizes.raw_bytes,
            minified_bytes: minified.len() as u64 + asset_sizes.minified_bytes,
            gzip_bytes: compressed.gzip_bytes + asset_sizes.gzip_bytes,
            brotli_bytes: compressed.brotli_bytes + asset_sizes.brotli_bytes,
            zstd_bytes: compressed.zstd_bytes + asset_sizes.zstd_bytes,
        },
    );
    result.side_effects = side_effects;
    result.truly_treeshakeable = truly_treeshakeable;
    result.is_cjs = is_cjs;
    result.confidence = confidence;
    result.confidence_reasons = confidence_reasons;
    result.diagnostics = diagnostics;
    result.module_breakdown = Some(top_module_contributions(&contributions));
    // Composition only: these bytes are already in the five sizes; this says which are
    // stylesheet, wasm, or font.
    result.asset_breakdown = assets.contributions;
    result.internal_contributions = contributions;

    Ok((result, loaded_paths, freshness))
}

pub(crate) fn engine_selection(request: &ImportRequest) -> crate::engine::BundleSelection {
    use crate::engine::BundleSelection;
    match request.import_kind {
        ImportKind::Named if !request.named.is_empty() => {
            BundleSelection::Named(request.named.clone())
        }
        // No requested names known: measure the full surface conservatively.
        ImportKind::Named => BundleSelection::Full,
        ImportKind::Default => BundleSelection::Default,
        ImportKind::Namespace => BundleSelection::Namespace,
        ImportKind::Dynamic => BundleSelection::Full,
    }
}

/// The contract's failure stages are a closed vocabulary; keep the known ones verbatim so
/// cache and diagnostic consumers see stable stage names, and collapse anything unknown to
/// `generate` rather than inventing a label.
///
/// The vocabulary is derived from `engine::stage::ALL`, never restated here: a restated list
/// drifts, and a missing stage (`panic`, `timeout`, `engine_gone`) would be relabelled as a
/// codegen failure while `file_size.rs` passes it through, giving one failure two names.
fn contract_stage(stage: &str) -> &'static str {
    crate::engine::stage::ALL
        .iter()
        .copied()
        .find(|known| *known == stage)
        .unwrap_or(crate::engine::stage::GENERATE)
}

fn engine_error(
    context: &AnalysisContext,
    request: &ImportRequest,
    failure: crate::engine::BundleFailure,
) -> AnalysisError {
    let mut error = error_with_context(
        contract_stage(&failure.stage),
        failure.message,
        context,
        request,
        failure
            .diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.stage, diagnostic.message))
            .collect(),
    );
    // The bytes the build read before it gave up. A cached deterministic failure expires against
    // exactly these (see `FailureFreshness::read_time_fingerprints`).
    error.freshness.read_time_fingerprints = failure.read_time_fingerprints;
    error.freshness.loaded_paths = failure.loaded_paths;
    error
}

fn asset_processing_error(
    context: &AnalysisContext,
    request: &ImportRequest,
    artifact: &crate::engine::BundleArtifact,
    failure: AssetProcessingFailure,
) -> AnalysisError {
    let mut error =
        error_with_context(failure.stage, failure.message, context, request, Vec::new());
    error.freshness.read_time_fingerprints = artifact.read_time_fingerprints.clone();
    error
        .freshness
        .read_time_fingerprints
        .extend(failure.read_time_fingerprints);
    error
        .freshness
        .read_time_fingerprints
        .extend(unhashed_fingerprints(&artifact.unhashed_paths));
    crate::cache::key::sort_and_dedup_fingerprints(&mut error.freshness.read_time_fingerprints);
    error.freshness.loaded_paths = artifact.loaded_paths.clone();
    error.freshness.loaded_paths.extend(failure.read_paths);
    error.freshness.loaded_paths.sort();
    error.freshness.loaded_paths.dedup();
    error
}

/// Modules whose bytes are in the build but whose read the plugin could not fingerprint, recorded
/// as **unverifiable**: see [`import_freshness`] for why they are never `stat_paths`.
fn unhashed_fingerprints(
    unhashed_paths: &[PathBuf],
) -> impl Iterator<Item = crate::cache::key::FileFingerprint> + '_ {
    unhashed_paths
        .iter()
        .map(crate::cache::key::unverifiable_file_fingerprint)
}

/// §8.3: freshness comes from fingerprints captured by the same reads that supplied every measured
/// byte. The plugin owns JavaScript and directly imported asset snapshots; the asset processor owns
/// CSS `@import` children and local resources discovered through `url()`.
///
/// `unhashed_paths` are modules whose bytes are in the number but whose read the plugin could not
/// fingerprint (a binary module Rolldown loaded itself). They are recorded as **unverifiable**,
/// never as `stat_paths`: `stat_paths` are hashed after the analysis, so a module rewritten inside
/// the analysis window would pair a size from the old bytes with a hash of the new ones and be
/// served as Fresh until the file changed again. An unverifiable fingerprint is never fresh.
///
/// A manifest belongs in `stat_paths`: it is a resolution input, not bytes inside the measured
/// chunk, so a post-analysis hash of it cannot contradict the size.
fn import_freshness(
    read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    unhashed_paths: &[PathBuf],
    asset_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    stat_paths: Vec<PathBuf>,
) -> FingerprintSource {
    let mut fingerprints = read_time_fingerprints;
    fingerprints.extend(unhashed_fingerprints(unhashed_paths));
    fingerprints.extend(asset_fingerprints);
    crate::cache::key::sort_and_dedup_fingerprints(&mut fingerprints);
    FingerprintSource::ReadTime {
        fingerprints,
        stat_paths,
    }
}

fn top_module_contributions(contributions: &[ModuleContribution]) -> Vec<ModuleContribution> {
    let mut contributions = contributions.to_vec();
    contributions.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    contributions.truncate(10);
    contributions
}

fn engine_confidence(
    side_effects: bool,
    diagnostics: &[ImportDiagnostic],
) -> (ConfidenceLevel, Vec<String>) {
    if !side_effects && diagnostics.is_empty() {
        return (
            ConfidenceLevel::High,
            vec![
                "Rolldown linking plus OXC validation, minification, and compression completed without precision warnings."
                    .to_owned(),
            ],
        );
    }

    let mut reasons = Vec::new();
    if side_effects {
        // The engine builds the same named-selection entry either way; an effectful package
        // just cannot be certified as fully tree-shakeable.
        reasons.push(
            "Package declares side effects, so modules it retains cannot be certified as \
             tree-shaken away."
                .to_owned(),
        );
    }

    if !diagnostics.is_empty() {
        let mut stages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.stage.as_str())
            .collect::<Vec<_>>();
        stages.sort_unstable();
        stages.dedup();
        reasons.push(format!(
            "Analysis emitted diagnostics that can reduce precision: {}.",
            stages.join(", ")
        ));
    }

    if reasons.is_empty() {
        reasons.push("Engine pipeline completed with conservative assumptions.".to_owned());
    }

    (ConfidenceLevel::Medium, reasons)
}

/// The Unmeasured result an analysis failure becomes.
///
/// It carries no size, never zeros (`0 B` reads as "this import is free"); the stage says why.
fn error_result(request: &ImportRequest, error: AnalysisError) -> ImportResult {
    ImportResult::unmeasured(
        request.specifier.clone(),
        error.stage,
        error.message,
        error.details,
    )
}

fn resolver_details(message: &str) -> Vec<String> {
    message
        .split("; ")
        .filter(|part| part.starts_with("checked:") || part.starts_with("candidate:"))
        .map(str::to_owned)
        .collect()
}

fn error_with_context(
    stage: &'static str,
    message: impl Into<String>,
    context: &AnalysisContext,
    request: &ImportRequest,
    details: Vec<String>,
) -> AnalysisError {
    let mut context_details = vec![
        format!("specifier: {}", request.specifier),
        format!("package: {}", request.package_name),
        format!(
            "active_document_path: {}",
            context.active_document_path.display()
        ),
        format!("workspace_root: {}", context.workspace_root.display()),
    ];
    context_details.extend(details);

    AnalysisError {
        stage,
        message: message.into(),
        details: context_details,
        freshness: Box::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module whose bytes were measured but whose read was never fingerprinted must be recorded
    /// as unverifiable, NOT deferred to a post-analysis stat.
    ///
    /// A post-analysis stat of a module rewritten inside the window pairs a v1 size with a v2
    /// hash, which every later probe answers Fresh.
    #[test]
    fn a_measured_but_unfingerprinted_module_is_unverifiable_not_stat_deferred() {
        let unhashed = PathBuf::from("/pkg/native.node");
        let manifest = PathBuf::from("/pkg/package.json");

        let freshness = import_freshness(
            Vec::new(),
            std::slice::from_ref(&unhashed),
            Vec::new(),
            vec![manifest.clone()],
        );

        let FingerprintSource::ReadTime {
            fingerprints,
            stat_paths,
        } = freshness;

        assert!(
            !stat_paths.contains(&unhashed),
            "a measured module must never be deferred to a post-analysis hash: {stat_paths:?}"
        );
        assert_eq!(
            stat_paths,
            vec![manifest],
            "only resolution inputs belong in stat_paths"
        );

        let recorded = fingerprints
            .iter()
            .find(|fingerprint| fingerprint.path.contains("native.node"))
            .expect("the measured module must still reach freshness at all");
        assert!(
            crate::cache::key::fingerprint_is_unverifiable(recorded),
            "it must be unverifiable so the result can never be served as fresh: {recorded:?}"
        );
    }

    /// The asset-failure path records the same unfingerprinted modules the same way the success
    /// path does: unverifiable, never deferred to a hash taken after the analysis.
    #[test]
    fn an_asset_stage_failure_records_unfingerprinted_modules_as_unverifiable() {
        let unhashed = PathBuf::from("/pkg/native.node");
        let artifact = crate::engine::BundleArtifact {
            code: String::new(),
            graph_source_bytes: 0,
            loaded_paths: vec![unhashed.clone()],
            read_time_fingerprints: Vec::new(),
            unhashed_paths: vec![unhashed],
            contributions: Vec::new(),
            exported_names: Vec::new(),
            diagnostics: Vec::new(),
            assets: Vec::new(),
            emitted_assets: Vec::new(),
        };
        let failure = AssetProcessingFailure {
            stage: crate::engine::stage::TIMEOUT,
            message: "asset processing did not complete within its deadline".to_owned(),
            read_paths: Vec::new(),
            read_time_fingerprints: Vec::new(),
        };
        let context = AnalysisContext {
            workspace_root: PathBuf::from("/workspace"),
            active_document_path: PathBuf::from("/workspace/src/index.ts"),
        };
        let request = ImportRequest {
            specifier: "pkg".to_owned(),
            package_name: "pkg".to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Namespace,
            runtime: crate::ipc::protocol::ImportRuntime::Component,
        };

        let error = asset_processing_error(&context, &request, &artifact, failure);

        let Some(FingerprintSource::ReadTime {
            fingerprints,
            stat_paths,
        }) = engine_failure_fingerprints(&context, Path::new("/pkg"), &error)
        else {
            panic!("an engine-built failure must carry freshness inputs");
        };
        assert!(
            !stat_paths.iter().any(|path| path.ends_with("native.node")),
            "a measured module must never be deferred to a post-analysis hash: {stat_paths:?}"
        );
        assert!(
            fingerprints.iter().any(|fingerprint| {
                fingerprint.path.contains("native.node")
                    && crate::cache::key::fingerprint_is_unverifiable(fingerprint)
            }),
            "{fingerprints:?}"
        );
    }

    /// Editing a first-party workspace dependency's manifest changes what the bundler
    /// resolves and retains, while none of its source files move. Without the manifest
    /// in the fingerprint set the cached size is served as fresh (§8.3).
    ///
    /// Loaded paths are canonicalized, so a workspace package linked into
    /// `node_modules` (as pnpm does) resolves to its real path and is correctly seen
    /// as first-party.
    #[test]
    fn first_party_manifests_are_fingerprint_inputs_and_installed_ones_are_not() {
        let workspace = std::env::temp_dir().join(format!(
            "import-lens-r5-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let ui = workspace.join("packages").join("ui");
        std::fs::create_dir_all(ui.join("src")).expect("ui src");
        std::fs::write(ui.join("package.json"), r#"{"name":"ui"}"#).expect("ui manifest");
        std::fs::write(ui.join("src").join("index.ts"), "export const a = 1;\n")
            .expect("ui source");

        let installed = workspace.join("node_modules").join("left-pad");
        std::fs::create_dir_all(&installed).expect("installed dir");
        std::fs::write(installed.join("package.json"), r#"{"name":"left-pad"}"#)
            .expect("installed manifest");
        std::fs::write(installed.join("index.js"), "export const b = 2;\n")
            .expect("installed source");

        let context = AnalysisContext {
            workspace_root: workspace.clone(),
            active_document_path: workspace.join("src").join("app.ts"),
        };

        let manifests = first_party_manifests(
            &context,
            &[
                ui.join("src").join("index.ts"),
                installed.join("index.js"),
                // A second module in the same package must not duplicate the manifest.
                ui.join("src").join("other.ts"),
            ],
        );

        assert_eq!(
            manifests,
            vec![ui.join("package.json")],
            "the first-party dependency's manifest is a freshness input; an installed \
             package's is covered by the install generation and must not be"
        );

        std::fs::remove_dir_all(&workspace).ok();
    }

    /// The other half of `contract_stage`: a stage the vocabulary does not know collapses to
    /// `generate` rather than reaching the client under an invented label.
    ///
    /// No companion test asserts that every stage in `ALL` survives: `contract_stage` searches
    /// `ALL`, so it would be identity by construction. `engine::stage` emits the constants and
    /// `ALL` from one macro invocation, so a stage missing from `ALL` cannot be written.
    #[test]
    fn an_unknown_stage_collapses_to_generate() {
        assert_eq!(
            contract_stage("something-nobody-defined"),
            crate::engine::stage::GENERATE,
            "an unrecognized stage collapses rather than inventing a label"
        );
    }
}
