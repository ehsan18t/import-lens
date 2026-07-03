use crate::{
    ipc::protocol::{
        ConfidenceLevel, ImportDiagnostic, ImportKind, ImportRequest, ImportResult,
        ModuleContribution,
    },
    pipeline::{
        bundle::bundle_reachable_modules_with_metadata,
        cjs::{CjsGraphAnalysis, analyze_cjs_graph_with_runtime},
        compress::compress_all,
        fallback::{approximate_directory_size, estimate_minified_source, source_excerpt_detail},
        graph::{
            MAX_MODULE_SOURCE_BYTES, ModuleGraph, build_module_graph_cached_with_runtime,
            module_provides_export,
        },
        minify::{minify_source, minify_source_with_markers},
        reachability::{reachable_exports, requested_exports},
        resolver::{ResolvedPackage, SideEffectsMode, find_package_root, resolve_package_entry},
        types_only::declaration_only_package_result,
    },
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct AnalysisContext {
    pub workspace_root: PathBuf,
    pub active_document_path: PathBuf,
}

#[derive(Debug, Clone)]
struct AnalysisError {
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
    match analyze_import_inner_resolved(context, request, resolved) {
        Ok(result) => result,
        Err(error) => error_result(request, error),
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
    analyze_import_inner_resolved(context, request, resolved)
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
) -> Result<ImportResult, AnalysisError> {
    let side_effects_mode = resolved.side_effects;
    let entry_path = resolved.entry_path;
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

    let entry_size = metadata.len() as usize;
    if entry_size > MAX_MODULE_SOURCE_BYTES {
        let entry_path_display = entry_path.display().to_string();
        let mut result =
            analyze_static_entry(context, request, entry_path, &side_effects_mode, is_cjs)?;
        result.diagnostics.insert(
            0,
            ImportDiagnostic {
                stage: "oversized_entry".to_owned(),
                message: format!(
                    "entry file exceeds {MAX_MODULE_SOURCE_BYTES} byte module source limit; skipped graph analysis and used static entry sizing"
                ),
                details: vec![format!("entry_path: {entry_path_display}")],
            },
        );
        result.confidence_reasons.insert(
            0,
            "Entry exceeds module graph source limit; size is static entry fallback.".to_owned(),
        );
        return Ok(result);
    }

    let mut fallback_diagnostics = Vec::new();
    if is_cjs {
        match analyze_with_cjs_graph(request, &entry_path) {
            Ok(result) => return Ok(result),
            Err(diagnostics) => fallback_diagnostics.extend(diagnostics),
        }
    }

    if !is_cjs {
        match analyze_with_oxc_pipeline(context, request, entry_path.clone(), &side_effects_mode) {
            Ok(result) => return Ok(result),
            Err(error) => fallback_diagnostics.push(oxc_fallback_diagnostic(error)),
        }
    }

    let mut result =
        analyze_static_entry(context, request, entry_path, &side_effects_mode, is_cjs)?;
    result.diagnostics.extend(fallback_diagnostics);
    Ok(result)
}

fn analyze_with_cjs_graph(
    request: &ImportRequest,
    entry_path: &Path,
) -> Result<ImportResult, Vec<ImportDiagnostic>> {
    let graph = analyze_cjs_graph_with_runtime(entry_path, request.runtime).map_err(|error| {
        vec![ImportDiagnostic {
            stage: "cjs_fallback".to_owned(),
            message: format!("CommonJS static analysis failed; using static entry sizing: {error}"),
            details: vec![format!("entry_path: {}", entry_path.display())],
        }]
    })?;

    if graph.unsupported {
        let mut diagnostics = graph.diagnostics;
        diagnostics.push(cjs_fallback_diagnostic(
            "unsupported dynamic CommonJS require; using static entry sizing".to_owned(),
            entry_path,
        ));
        return Err(diagnostics);
    }
    if graph.exports.is_empty() {
        let mut diagnostics = graph.diagnostics;
        diagnostics.push(cjs_fallback_diagnostic(
            "unsupported CommonJS export shape; using static entry sizing".to_owned(),
            entry_path,
        ));
        return Err(diagnostics);
    }

    Ok(cjs_graph_result(request, graph))
}

fn cjs_graph_result(request: &ImportRequest, graph: CjsGraphAnalysis) -> ImportResult {
    let minified = minify_source(&graph.source, true)
        .unwrap_or_else(|_| estimate_minified_source(&graph.source));
    let compressed = compress_all(&minified);
    let mut diagnostics = graph.diagnostics;
    diagnostics.extend(missing_cjs_export_diagnostics(request, &graph.exports));
    let module_breakdown = top_module_contributions(&graph.module_breakdown);
    let internal_contributions = graph.full_module_breakdown;

    match compressed {
        Ok(compressed) => ImportResult {
            specifier: request.specifier.clone(),
            raw_bytes: graph.source.len() as u64,
            minified_bytes: minified.len() as u64,
            gzip_bytes: compressed.gzip_bytes,
            brotli_bytes: compressed.brotli_bytes,
            zstd_bytes: compressed.zstd_bytes,
            cache_hit: false,
            side_effects: true,
            truly_treeshakeable: false,
            is_cjs: true,
            confidence: ConfidenceLevel::Low,
            confidence_reasons: vec![
                "CommonJS static analysis is conservative and may miss dynamic require or runtime export behavior."
                    .to_owned(),
            ],
            error: None,
            diagnostics,
            module_breakdown: Some(module_breakdown),
            shared_bytes: None,
            internal_contributions,
        },
        Err(error) => error_result(
            request,
            AnalysisError {
                stage: "compression",
                message: format!("failed to compress CommonJS graph: {error}"),
                details: Vec::new(),
            },
        ),
    }
}

fn analyze_with_oxc_pipeline(
    context: &AnalysisContext,
    request: &ImportRequest,
    entry_path: PathBuf,
    side_effects_mode: &SideEffectsMode,
) -> Result<ImportResult, AnalysisError> {
    let graph =
        build_module_graph_cached_with_runtime(&entry_path, request.runtime).map_err(|error| {
            error_with_context(
                "module_graph",
                format!("failed to build module graph: {error}"),
                context,
                request,
                vec![format!("entry_path: {}", entry_path.display())],
            )
        })?;
    let side_effect_matches =
        side_effects_mode.matching_paths(graph.modules.iter().map(|module| module.path.as_path()));
    let side_effects = side_effects_mode.has_side_effects() || !side_effect_matches.is_empty();
    let include_full_entry = side_effects_mode.has_side_effects()
        || matches!(
            request.import_kind,
            ImportKind::Namespace | ImportKind::Dynamic
        );
    let requested_exports = requested_exports(request);
    let mut reachable = reachable_exports(&graph, &requested_exports, include_full_entry);
    for path in &side_effect_matches {
        reachable.mark_full_module(path.clone());
    }
    let mut bundled =
        bundle_reachable_modules_with_metadata(&graph, &reachable).map_err(|error| {
            error_with_context(
                "bundle",
                format!("failed to bundle reachable modules: {error}"),
                context,
                request,
                vec![format!("entry_path: {}", entry_path.display())],
            )
        })?;
    let mut fallback_full_bundle = false;
    if bundled.source.trim().is_empty() && !include_full_entry {
        reachable = reachable_exports(&graph, &[], true);
        bundled = bundle_reachable_modules_with_metadata(&graph, &reachable).map_err(|error| {
            error_with_context(
                "bundle",
                format!("failed to bundle fallback full module: {error}"),
                context,
                request,
                vec![format!("entry_path: {}", entry_path.display())],
            )
        })?;
        fallback_full_bundle = true;
    }
    let minified =
        minify_source_with_markers(&bundled.minifier_source, false).map_err(|error| {
            error_with_context(
                "minify",
                format!("failed to minify bundled modules: {error}"),
                context,
                request,
                vec![
                    format!("entry_path: {}", entry_path.display()),
                    source_excerpt_detail(&bundled.minifier_source),
                ],
            )
        })?;
    if fallback_full_bundle {
        graph.cache_full_bundle_minified_len(minified.len() as u64);
    }
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            "compression",
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;

    let mut diagnostics =
        side_effect_diagnostics(side_effects_mode, &entry_path, &side_effect_matches);
    diagnostics.extend(graph.diagnostics.iter().map(|diagnostic| ImportDiagnostic {
        stage: diagnostic.stage.clone(),
        message: diagnostic.message.clone(),
        details: diagnostic.details.clone(),
    }));
    diagnostics.extend(missing_export_diagnostics(request, &graph));
    let (confidence, confidence_reasons) = oxc_confidence(side_effects, &diagnostics);

    Ok(ImportResult {
        specifier: request.specifier.clone(),
        raw_bytes: bundled.source.len() as u64,
        minified_bytes: minified.len() as u64,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        cache_hit: false,
        side_effects,
        truly_treeshakeable: is_truly_treeshakeable(
            request,
            side_effects,
            &graph,
            minified.len() as u64,
            fallback_full_bundle.then_some(minified.len() as u64),
        ),
        is_cjs: false,
        confidence,
        confidence_reasons,
        error: None,
        diagnostics,
        module_breakdown: Some(top_module_contributions(&bundled.contributions)),
        shared_bytes: None,
        internal_contributions: bundled.contributions,
    })
}

fn is_truly_treeshakeable(
    request: &ImportRequest,
    side_effects: bool,
    graph: &crate::pipeline::graph::ModuleGraph,
    minified_len: u64,
    cached_full_minified_len: Option<u64>,
) -> bool {
    if side_effects || !matches!(request.import_kind, ImportKind::Named) {
        return false;
    }

    // To check if genuinely tree-shakeable, compare against the same post-minify
    // size surface that users see in ImportLens.
    let full_len = cached_full_minified_len
        .or_else(|| {
            graph.cached_full_bundle_minified_len_or_init(|| {
                let reachable_full = reachable_exports(graph, &[], true);
                let bundled_full =
                    bundle_reachable_modules_with_metadata(graph, &reachable_full).ok()?;
                let minified_full =
                    minify_source_with_markers(&bundled_full.minifier_source, false).ok()?;
                Some(minified_full.len() as u64)
            })
        })
        .unwrap_or_default();
    if full_len == 0 {
        return false;
    }

    // If the tree-shaken size is within 5% of the full size, it's not truly tree-shakeable
    let ratio = (minified_len as f64) / (full_len as f64);
    ratio <= 0.95
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

fn oxc_confidence(
    side_effects: bool,
    diagnostics: &[ImportDiagnostic],
) -> (ConfidenceLevel, Vec<String>) {
    if !side_effects && diagnostics.is_empty() {
        return (
            ConfidenceLevel::High,
            vec![
                "OXC module graph, transform, minify, and compression completed without precision warnings."
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
        reasons.push("OXC pipeline completed with conservative assumptions.".to_owned());
    }

    (ConfidenceLevel::Medium, reasons)
}

fn oxc_fallback_diagnostic(error: AnalysisError) -> ImportDiagnostic {
    let mut details = vec![format!("failed_stage: {}", error.stage)];
    details.extend(error.details);

    ImportDiagnostic {
        stage: "oxc_fallback".to_owned(),
        message: format!(
            "OXC pipeline failed; using static entry sizing: {}",
            error.message
        ),
        details,
    }
}

fn cjs_fallback_diagnostic(message: String, entry_path: &Path) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: "cjs_fallback".to_owned(),
        message,
        details: vec![format!("entry_path: {}", entry_path.display())],
    }
}

fn missing_export_diagnostics(
    request: &ImportRequest,
    graph: &ModuleGraph,
) -> Vec<ImportDiagnostic> {
    let Some(requested_exports) = diagnostic_requested_exports(request) else {
        return Vec::new();
    };

    let missing = requested_exports
        .iter()
        .filter(|exported_name| {
            !module_provides_export(graph, graph.entry_id, exported_name, &mut HashSet::new())
        })
        .cloned()
        .collect::<Vec<_>>();

    if missing.is_empty() {
        return Vec::new();
    }

    vec![ImportDiagnostic {
        stage: "exports".to_owned(),
        message: missing_export_message(request, &missing),
        details: vec![
            format!("specifier: {}", request.specifier),
            format!("missing_exports: {}", missing.join(", ")),
        ],
    }]
}

fn missing_cjs_export_diagnostics(
    request: &ImportRequest,
    exports: &[String],
) -> Vec<ImportDiagnostic> {
    let Some(requested_exports) = diagnostic_requested_exports(request) else {
        return Vec::new();
    };

    let missing = requested_exports
        .iter()
        .filter(|name| !exports.contains(name))
        .cloned()
        .collect::<Vec<_>>();

    if missing.is_empty() {
        return Vec::new();
    }

    vec![ImportDiagnostic {
        stage: "exports".to_owned(),
        message: missing_cjs_export_message(request, &missing),
        details: vec![
            format!("specifier: {}", request.specifier),
            format!("missing_exports: {}", missing.join(", ")),
        ],
    }]
}

fn diagnostic_requested_exports(request: &ImportRequest) -> Option<Vec<String>> {
    match request.import_kind {
        ImportKind::Named => Some(request.named.clone()),
        ImportKind::Default => Some(vec!["default".to_owned()]),
        ImportKind::Namespace | ImportKind::Dynamic => None,
    }
}

fn missing_export_message(request: &ImportRequest, missing: &[String]) -> String {
    match request.import_kind {
        ImportKind::Default => "default export not found".to_owned(),
        _ => format!("named export(s) not found: {}", missing.join(", ")),
    }
}

fn missing_cjs_export_message(request: &ImportRequest, missing: &[String]) -> String {
    match request.import_kind {
        ImportKind::Default => "default CommonJS export not found".to_owned(),
        _ => format!("named CommonJS export(s) not found: {}", missing.join(", ")),
    }
}

fn error_result(request: &ImportRequest, error: AnalysisError) -> ImportResult {
    ImportResult {
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
