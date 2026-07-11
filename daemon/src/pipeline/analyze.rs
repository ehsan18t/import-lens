use crate::{
    engine::{dependency_paths::record_loaded_paths, limits::MAX_MODULE_SOURCE_BYTES},
    ipc::protocol::{
        ConfidenceLevel, ImportDiagnostic, ImportKind, ImportRequest, ImportResult,
        ModuleContribution, ResultFreshness,
    },
    pipeline::{
        compress::compress_all,
        fallback::{approximate_directory_size, estimate_minified_source, source_excerpt_detail},
        minify::minify_source,
        resolver::{ResolvedPackage, SideEffectsMode, find_package_root, resolve_package_entry},
        types_only::declaration_only_package_result,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// Paths used to compute cache freshness for a successful engine build.
pub enum FingerprintSource {
    LoadedPaths(Vec<PathBuf>),
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
        Ok((result, loaded_paths)) => {
            record_loaded_paths(entry_path, request.runtime, loaded_paths.clone());
            Ok((result, Some(FingerprintSource::LoadedPaths(loaded_paths))))
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
) -> Result<(ImportResult, Vec<PathBuf>), AnalysisError> {
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
    let mut truly_treeshakeable = false;
    if !side_effects
        && matches!(request.import_kind, ImportKind::Named)
        && !request.named.is_empty()
    {
        match boundary::bundle_sync(BundleRequest {
            entries: vec![bundle_entry(BundleSelection::Full)],
            runtime: request.runtime,
            purpose: BundlePurpose::FullPackageComparison,
        }) {
            Ok(full) => {
                if let Ok(full_minified) = minify_source(&full.code, false) {
                    let full_len = full_minified.len() as u64;
                    if full_len > 0 {
                        // Mirror the legacy predicate: within 5% of the full
                        // size is not truly tree-shakeable.
                        let ratio = (minified.len() as f64) / (full_len as f64);
                        truly_treeshakeable = ratio <= 0.95;
                    }
                }
            }
            Err(failure) => diagnostics.push(ImportDiagnostic {
                stage: "full_package_comparison".to_owned(),
                message: format!(
                    "full-package comparison build failed; treating as not tree-shakeable: {}",
                    failure.message
                ),
                details: Vec::new(),
            }),
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

    // §8.3: manifests used for resolution/side-effect classification join
    // the fingerprint inputs alongside every loaded source path.
    let mut loaded_paths = artifact.loaded_paths;
    loaded_paths.push(package_root.join("package.json"));

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
    Ok((result, loaded_paths))
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

fn engine_error(
    context: &AnalysisContext,
    request: &ImportRequest,
    failure: crate::engine::BundleFailure,
) -> AnalysisError {
    // The contract's failure stages are a closed vocabulary; keep them
    // verbatim so cache/diagnostic consumers see stable stage names.
    let stage = match failure.stage.as_str() {
        "resolve" => "resolve",
        "parse" => "parse",
        "link" => "link",
        "output_shape" => "output_shape",
        "module_graph_limit" => "module_graph_limit",
        "missing_export" => "missing_export",
        "ambiguous_export" => "ambiguous_export",
        _ => "generate",
    };
    error_with_context(
        stage,
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

fn engine_fallback_diagnostic(error: AnalysisError) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: "engine_fallback".to_owned(),
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
        reasons.push(
            "Package side effects require full-graph sizing instead of named-export tree shaking."
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
