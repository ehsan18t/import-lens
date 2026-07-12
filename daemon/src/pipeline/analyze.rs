use crate::{
    engine::{dependency_paths::record_loaded_paths, limits::MAX_MODULE_SOURCE_BYTES},
    ipc::protocol::{
        ConfidenceLevel, ImportDiagnostic, ImportKind, ImportRequest, ImportResult,
        ModuleContribution, ResultFreshness,
    },
    pipeline::{
        compress::compress_all,
        fallback::{approximate_directory_size, estimate_minified_source, source_excerpt_detail},
        full_package,
        minify::minify_source,
        resolver::{ResolvedPackage, SideEffectsMode, find_package_root, resolve_package_entry},
        types_only::declaration_only_package_result,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Where an analysis runs, and nothing else.
///
/// It used to carry an engine deadline too, because the response an import rode in was atomic:
/// one build that parked pushed a whole document's results past the client's patience, so the
/// request had to be able to abandon builds. It no longer is — a request answers from cache and
/// each build is pushed to the client as it lands (`ipc::server`) — so no build has a deadline
/// to be measured against, and none is passed one.
#[derive(Debug, Clone)]
pub struct AnalysisContext {
    pub workspace_root: PathBuf,
    pub active_document_path: PathBuf,
}

// Internal structured error translated into the stable ImportResult surface.
#[derive(Debug, Clone)]
pub struct AnalysisError {
    stage: &'static str,
    message: String,
    details: Vec<String>,
}

pub fn analyze_import(context: &AnalysisContext, request: &ImportRequest) -> ImportResult {
    match analyze_import_inner(context, request) {
        Ok(result) => result,
        Err(error) => error_result(request, error),
    }
}

pub fn analyze_resolved_import(
    context: &AnalysisContext,
    request: &ImportRequest,
    resolved: ResolvedPackage,
) -> ImportResult {
    analyze_resolved_import_with_dependencies(context, request, resolved).0
}

/// Manifests of the first-party packages whose sources this build loaded (§8.3).
///
/// The plugin records graph *modules*, and a `package.json` is never one — but it
/// drives resolution and side-effect classification, so editing a workspace
/// dependency's `exports`, `type` or `sideEffects` changes what the bundler pulls in
/// while no fingerprinted path moves, and a stale size is served as fresh. The dep's
/// *source* files are fingerprinted, so editing its code is already caught; what is
/// missed is editing its manifest.
///
/// Installed packages are excluded: their manifests cannot change without an install,
/// which bumps the cache generation, and including them would balloon the fingerprint
/// set for every build.
fn first_party_manifests(context: &AnalysisContext, loaded_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Loaded paths are canonicalized (verbatim `\\?\C:\...` on Windows) while the
    // workspace root arrives off the wire as the editor spelled it, so the two never
    // compare equal unless the root is canonicalized too — and the walk below would
    // not actually stop where this says it does.
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

/// Everything the full-package memo must expire against: the read-time fingerprints
/// of every module the comparison build measured, plus the manifests that decide what
/// it resolved. Mirrors the freshness set the import cache stores for the entry build
/// itself — if the two ever diverge, the memo would outlive the size it describes.
fn full_package_fingerprints(
    context: &AnalysisContext,
    package_root: &Path,
    full: &crate::engine::BundleArtifact,
) -> Vec<crate::cache::key::FileFingerprint> {
    use crate::cache::key::file_fingerprint_reading_hash;

    let mut fingerprints = full.read_time_fingerprints.clone();
    fingerprints.extend(
        std::iter::once(package_root.join("package.json"))
            .chain(first_party_manifests(context, &full.loaded_paths))
            .filter_map(file_fingerprint_reading_hash),
    );
    fingerprints
}

/// Freshness inputs for a successful engine build (§8.3).
///
/// `fingerprints` were captured as each module's bytes were read *during* the
/// build, so they describe exactly the bytes the size was measured from. Anything
/// that is not a graph module — the package manifest, and binary modules the plugin
/// handed back to Rolldown — has no read-time capture and is listed in `stat_paths`
/// for the caller to hash.
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
    match analyze_import_inner_resolved(context, request, resolved) {
        Ok((result, source)) => (result, source),
        Err(error) => (error_result(request, error), None),
    }
}

fn analyze_import_inner(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ImportResult, AnalysisError> {
    let resolved = match resolve_import_package(context, request) {
        Ok(resolved) => resolved,
        Err(error) if error.stage == "package_manifest" => {
            return approximate_manifest_fallback(context, request, error);
        }
        Err(error) if error.stage == "entry_resolution" => {
            if let Some(result) =
                declaration_only_package_result(&context.active_document_path, request)
            {
                return Ok(result);
            }

            return Err(error);
        }
        Err(error) => return Err(error),
    };
    // The non-resolved path has no caller that needs the analyzed graph, so it
    // discards it and keeps returning a bare `ImportResult`.
    let (result, _graph) = analyze_import_inner_resolved(context, request, resolved)?;
    Ok(result)
}

fn resolve_import_package(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ResolvedPackage, AnalysisError> {
    resolve_package_entry(&context.active_document_path, request).map_err(|message| {
        let stage = if message.contains("unsafe package name") {
            "package_validation"
        } else if message.contains("package manifest not found") {
            "package_resolution"
        } else if is_manifest_fallback_error(&message) {
            "package_manifest"
        } else {
            "entry_resolution"
        };
        let details = resolver_details(&message);
        error_with_context(stage, message, context, request, details)
    })
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

    let metadata = fs::metadata(&entry_path).map_err(|error| {
        error_with_context(
            "entry_metadata",
            format!(
                "failed to stat package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;

    if metadata.len() as usize > MAX_MODULE_SOURCE_BYTES {
        let entry_path_display = entry_path.display().to_string();
        let mut result =
            analyze_static_entry(context, request, entry_path, &side_effects_mode, is_cjs)?;
        result.diagnostics.insert(
            0,
            ImportDiagnostic {
                stage: "oversized_entry".to_owned(),
                message: format!(
                    "entry file exceeds {MAX_MODULE_SOURCE_BYTES} byte module source limit; used static entry sizing"
                ),
                details: vec![format!("entry_path: {entry_path_display}")],
            },
        );
        result.confidence_reasons.insert(
            0,
            "Entry exceeds the engine module source limit; size is a static fallback.".to_owned(),
        );
        return Ok((result, None));
    }

    match analyze_with_rolldown_engine(
        context,
        request,
        &entry_path,
        &package_root,
        &side_effects_mode,
        is_cjs,
    ) {
        Ok((result, loaded_paths, freshness)) => {
            record_loaded_paths(entry_path, request.runtime, loaded_paths);
            Ok((result, Some(freshness)))
        }
        Err(error) if matches!(error.stage, "missing_export" | "ambiguous_export") => Err(error),
        Err(error) => {
            let mut result =
                analyze_static_entry(context, request, entry_path, &side_effects_mode, is_cjs)?;
            result.diagnostics.push(engine_fallback_diagnostic(error));
            Ok((result, None))
        }
    }
}

/// Rolldown-backed analysis (spec §8): one engine build produces the raw
/// chunk, OXC minifies it, and the existing compression pipeline runs over
/// the minified string. Returns the loaded real paths (plus the package
/// manifest) for §8.3 freshness fingerprints alongside the result.
///
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
        reported_side_effects: side_effects_mode.clone(),
    };
    let artifact = boundary::bundle_sync(BundleRequest {
        entries: vec![bundle_entry(engine_selection(request))],
        runtime: request.runtime,
        purpose: BundlePurpose::ImportSize,
    })
    .map_err(|failure| engine_error(context, request, failure))?;

    let minified = minify_source(&artifact.code, false).map_err(|error| {
        error_with_context(
            "minify",
            format!("failed to minify engine chunk: {error}"),
            context,
            request,
            vec![source_excerpt_detail(&artifact.code)],
        )
    })?;
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            "compression",
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;

    // §7.4/§14.5: with glob matching unavailable from public bundler
    // metadata, an Array declaration reports conservatively as
    // side-effectful (legacy ORs in glob matches over graph modules; the
    // adapter's conservative-confidence diagnostic already flags this).
    let side_effects = side_effects_mode.has_side_effects() || side_effects_mode.is_array();
    let mut diagnostics: Vec<ImportDiagnostic> = artifact
        .diagnostics
        .iter()
        .map(|diagnostic| ImportDiagnostic {
            stage: diagnostic.stage.clone(),
            message: diagnostic.message.clone(),
            details: Vec::new(),
        })
        .collect();

    // Full-package comparison (§8.4/§6.3): a second engine build measures the
    // complete surface; failure degrades to "not treeshakeable", never an
    // analysis error.
    //
    // The answer does not depend on *which* names were imported, but the import
    // cache key does — so without the memo, N named variants of one entry cost N
    // of these builds on top of their own. `full_package::lookup` re-checks the
    // fingerprints of the exact bytes the stored length was measured from, so it
    // expires precisely when the length it holds would have gone wrong.
    let mut truly_treeshakeable = false;
    if !side_effects
        && matches!(request.import_kind, ImportKind::Named)
        && !request.named.is_empty()
    {
        let full_len = full_package::lookup(entry_path, request.runtime).or_else(|| {
            // Read before the build, not after: an invalidation landing while this build
            // is in flight must not be stamped onto a length measured from the bytes it
            // invalidated.
            let generation = crate::cache::memory::cache_generation();
            let full = match boundary::bundle_sync(BundleRequest {
                entries: vec![bundle_entry(BundleSelection::Full)],
                runtime: request.runtime,
                purpose: BundlePurpose::FullPackageComparison,
            }) {
                Ok(full) => full,
                Err(failure) => {
                    // Reported under the stage the comparison build actually failed at, not
                    // under a label invented here (§12, same rule as `engine_fallback_diagnostic`
                    // — the fallback is expressed by the message, not by erasing where it broke).
                    // That is also what lets `should_cache_result` see a TRANSIENT failure here:
                    // `truly_treeshakeable: false` is a fabricated fact when the build that would
                    // have disproved it merely timed out, and caching it would mark a healthy
                    // package "not tree-shakeable" for a whole cache generation.
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

            let full_len = minify_source(&full.code, false).ok()?.len() as u64;
            // A graph carrying a module the plugin could not fingerprint as it read it
            // (a binary module) has no complete read-time record, so there is nothing to
            // expire a memo against: measure it, use it, and store nothing.
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
            // Mirror the legacy predicate: within 5% of the full size is not
            // truly tree-shakeable.
            let ratio = (minified.len() as f64) / (full_len as f64);
            truly_treeshakeable = ratio <= 0.95;
        }
    }

    let (confidence, confidence_reasons) = engine_confidence(side_effects, &diagnostics);
    let contributions: Vec<ModuleContribution> = artifact
        .contributions
        .iter()
        .map(|contribution| ModuleContribution {
            path: contribution.path.to_string_lossy().to_string(),
            bytes: contribution.rendered_bytes as u64,
        })
        .collect();

    // §8.3: freshness comes from the fingerprints the plugin captured as it read each
    // module, so they describe the exact bytes this size was measured from. Two kinds
    // of input have no read-time capture and must still be hashed: the package
    // manifest, which drives resolution and side-effect classification but is not a
    // graph module, and any binary module the plugin handed back to Rolldown. Hashing
    // those after the build does not reopen the staleness window the read-time capture
    // closes — a manifest is an input to resolution, not a source of measured bytes.
    let mut stat_paths = artifact.unhashed_paths;
    stat_paths.push(package_root.join("package.json"));
    stat_paths.extend(first_party_manifests(context, &artifact.loaded_paths));
    let freshness = FingerprintSource::ReadTime {
        fingerprints: artifact.read_time_fingerprints,
        stat_paths,
    };
    let loaded_paths = artifact.loaded_paths;

    let result = ImportResult {
        freshness: ResultFreshness::fresh(),
        specifier: request.specifier.clone(),
        raw_bytes: artifact.code.len() as u64,
        minified_bytes: minified.len() as u64,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        cache_hit: false,
        side_effects,
        truly_treeshakeable,
        is_cjs,
        confidence,
        confidence_reasons,
        error: None,
        diagnostics,
        module_breakdown: Some(top_module_contributions(&contributions)),
        shared_bytes: None,
        internal_contributions: contributions,
    };
    Ok((result, loaded_paths, freshness))
}

pub(crate) fn engine_selection(request: &ImportRequest) -> crate::engine::BundleSelection {
    use crate::engine::BundleSelection;
    match request.import_kind {
        ImportKind::Named if !request.named.is_empty() => {
            BundleSelection::Named(request.named.clone())
        }
        // No requested names known: measure the full surface conservatively,
        // matching the legacy empty-bundle fallback.
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
/// The vocabulary is *derived* from `engine::stage::ALL` rather than restated here. It used
/// to be restated, and the restatement drifted: the boundary's `panic`, `timeout` and
/// `engine_gone` were missing, so a daemon-side panic reached the user relabelled as an
/// ordinary codegen failure — indistinguishable from one — while `file_size.rs` passed the
/// same stage through untouched, giving one failure two names in two different responses.
/// Deriving the list makes that class of drift impossible instead of merely testable.
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
    error_with_context(
        contract_stage(&failure.stage),
        failure.message,
        context,
        request,
        failure
            .diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.stage, diagnostic.message))
            .collect(),
    )
}

/// §12 requires each failure to surface under the stage it happened at — `parse`,
/// `resolve`, `link`, `generate`, `output_shape`, `module_graph_limit`, or the OXC
/// stage for a validation/minification failure. Overwriting every one of them with a
/// single `engine_fallback` label collapsed the whole failure table into one bucket
/// and left the real stage recoverable only by reading the message. The fallback is
/// expressed by the message and the lowered confidence, not by erasing where the
/// failure occurred.
fn engine_fallback_diagnostic(error: AnalysisError) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: error.stage.to_owned(),
        message: format!(
            "Rolldown engine analysis failed; using static entry sizing: {}",
            error.message
        ),
        details: error.details,
    }
}

fn analyze_static_entry(
    context: &AnalysisContext,
    request: &ImportRequest,
    entry_path: PathBuf,
    side_effects_mode: &SideEffectsMode,
    is_cjs: bool,
) -> Result<ImportResult, AnalysisError> {
    let side_effects = side_effects_mode.has_side_effects();
    let side_effect_matches =
        side_effects_mode.matching_paths(std::iter::once(entry_path.as_path()));
    let source = fs::read_to_string(&entry_path).map_err(|error| {
        error_with_context(
            "entry_read",
            format!(
                "failed to read package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;
    let minified =
        minify_source(&source, is_cjs).unwrap_or_else(|_| estimate_minified_source(&source));
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            "compression",
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;
    let raw_bytes = source.len() as u64;
    let minified_bytes = minified.len() as u64;

    Ok(ImportResult {
        freshness: ResultFreshness::fresh(),
        specifier: request.specifier.clone(),
        raw_bytes,
        minified_bytes,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        cache_hit: false,
        side_effects,
        truly_treeshakeable: false,
        is_cjs,
        confidence: ConfidenceLevel::Low,
        confidence_reasons: vec![
            "Static entry sizing is a fallback; it does not build a complete module graph."
                .to_owned(),
        ],
        error: None,
        diagnostics: side_effect_diagnostics(side_effects_mode, &entry_path, &side_effect_matches),
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
    })
}

fn approximate_manifest_fallback(
    context: &AnalysisContext,
    request: &ImportRequest,
    error: AnalysisError,
) -> Result<ImportResult, AnalysisError> {
    let package_root = find_package_root(&context.active_document_path, &request.package_name)
        .map_err(|message| {
            error_with_context("package_resolution", message, context, request, Vec::new())
        })?;
    let (raw_bytes, mut diagnostics) = approximate_directory_size(&package_root);
    diagnostics.insert(
        0,
        ImportDiagnostic {
            stage: "manifest_fallback".to_owned(),
            message: format!(
                "package manifest could not be used; computed approximate raw directory size (approx): {}",
                error.message
            ),
            details: vec![
                format!("package_root: {}", package_root.display()),
                format!("failed_stage: {}", error.stage),
            ],
        },
    );

    Ok(ImportResult {
        freshness: ResultFreshness::fresh(),
        specifier: request.specifier.clone(),
        raw_bytes,
        minified_bytes: raw_bytes,
        gzip_bytes: raw_bytes,
        brotli_bytes: raw_bytes,
        zstd_bytes: raw_bytes,
        cache_hit: false,
        side_effects: true,
        truly_treeshakeable: false,
        is_cjs: false,
        confidence: ConfidenceLevel::Low,
        confidence_reasons: vec![
            "Package manifest fallback uses approximate directory sizing because manifest resolution failed."
                .to_owned(),
        ],
        error: None,
        diagnostics,
        module_breakdown: Some(vec![ModuleContribution {
            path: package_root.to_string_lossy().to_string(),
            bytes: raw_bytes,
        }]),
        shared_bytes: None,
        internal_contributions: vec![ModuleContribution {
            path: package_root.to_string_lossy().to_string(),
            bytes: raw_bytes,
        }],
    })
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

pub(crate) fn side_effect_diagnostics(
    side_effects_mode: &SideEffectsMode,
    entry_path: &Path,
    matched_paths: &[PathBuf],
) -> Vec<ImportDiagnostic> {
    if !side_effects_mode.is_array() || matched_paths.is_empty() {
        return Vec::new();
    }

    let mut details = vec![
        "sideEffects: array".to_owned(),
        format!("entry_path: {}", entry_path.display()),
    ];
    details.extend(
        matched_paths
            .iter()
            .map(|path| format!("matched_path: {}", path.display())),
    );

    vec![ImportDiagnostic {
        stage: "side_effects".to_owned(),
        message: "package sideEffects array matched analyzed module path(s); conservative inclusion applied".to_owned(),
        details,
    }]
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
        // The engine builds the same named-selection entry whether or not the package
        // declares side effects; what changes is that an effectful package cannot be
        // certified as fully tree-shakeable. The old text described the deleted
        // engine, which really did switch to full-graph sizing here.
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

fn error_result(request: &ImportRequest, error: AnalysisError) -> ImportResult {
    ImportResult {
        freshness: ResultFreshness::fresh(),
        specifier: request.specifier.clone(),
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        cache_hit: false,
        side_effects: true,
        truly_treeshakeable: false,
        is_cjs: false,
        confidence: ConfidenceLevel::Low,
        confidence_reasons: vec![
            "Analysis failed before a bundle size could be measured.".to_owned(),
        ],
        error: Some(error.message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: error.stage.to_owned(),
            message: error.message,
            details: error.details,
        }],
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
    }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Editing a first-party workspace dependency's manifest changes what the bundler
    /// resolves and retains, while none of its source files move. Without the manifest
    /// in the fingerprint set the cached size is served as fresh (spec R5).
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
    /// There is deliberately no companion test asserting that every stage in `stage::ALL`
    /// survives the edge. `contract_stage` *searches* `ALL`, so such a test is identity over
    /// `ALL` by construction and can never go red — it would only look like coverage. The
    /// property it pretended to protect (a declared stage is in `ALL`) is now structural:
    /// `engine::stage` emits the constants and `ALL` from one macro invocation, so a stage
    /// that is missing from `ALL` cannot be written.
    #[test]
    fn an_unknown_stage_collapses_to_generate() {
        assert_eq!(
            contract_stage("something-nobody-defined"),
            crate::engine::stage::GENERATE,
            "an unrecognized stage collapses rather than inventing a label"
        );
    }
}
