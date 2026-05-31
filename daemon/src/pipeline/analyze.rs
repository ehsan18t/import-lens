use crate::{
    ipc::protocol::{ImportDiagnostic, ImportKind, ImportRequest, ImportResult},
    pipeline::{
        bundle::bundle_reachable_modules,
        compress::compress_all,
        graph::{ModuleGraph, ModuleId, build_module_graph_cached},
        minify::minify_source,
        reachability::reachable_exports,
        resolver::{ResolvedPackage, SideEffectsMode, resolve_package_entry},
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
    let resolved = resolve_import_package(context, request)?;
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
        } else {
            "entry_resolution"
        };
        let details = resolver_details(&message);
        error_with_context(stage, message, context, request, details)
    })
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

    let max_size = 5 * 1024 * 1024;
    if metadata.len() > max_size {
        return Err(error_with_context(
            "file_size_limit",
            format!("file size {} exceeds 5MB limit", metadata.len()),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        ));
    }

    let mut fallback_diagnostics = Vec::new();
    if !is_cjs
        && matches!(
            request.import_kind,
            ImportKind::Named | ImportKind::Default | ImportKind::Namespace
        )
    {
        match analyze_with_oxc_pipeline(context, request, entry_path.clone(), side_effects_mode) {
            Ok(result) => return Ok(result),
            Err(error) if matches!(request.import_kind, ImportKind::Namespace) => {
                fallback_diagnostics.push(oxc_fallback_diagnostic(error));
            }
            Err(error) => return Err(error),
        }
    }

    let mut result = analyze_static_entry(context, request, entry_path, side_effects_mode, is_cjs)?;
    result.diagnostics.extend(fallback_diagnostics);
    Ok(result)
}

fn analyze_with_oxc_pipeline(
    context: &AnalysisContext,
    request: &ImportRequest,
    entry_path: PathBuf,
    side_effects_mode: SideEffectsMode,
) -> Result<ImportResult, AnalysisError> {
    let side_effects = side_effects_mode.has_side_effects();
    let graph = build_module_graph_cached(&entry_path).map_err(|error| {
        error_with_context(
            "module_graph",
            format!("failed to build module graph: {error}"),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;
    let include_full_entry = side_effects || matches!(request.import_kind, ImportKind::Namespace);
    let requested_exports = requested_exports(request);
    let mut reachable = reachable_exports(&graph, &requested_exports, include_full_entry);
    let mut bundled = bundle_reachable_modules(&graph, &reachable).map_err(|error| {
        error_with_context(
            "bundle",
            format!("failed to bundle reachable modules: {error}"),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;
    if bundled.trim().is_empty() && !include_full_entry {
        reachable = reachable_exports(&graph, &[], true);
        bundled = bundle_reachable_modules(&graph, &reachable).map_err(|error| {
            error_with_context(
                "bundle",
                format!("failed to bundle fallback full module: {error}"),
                context,
                request,
                vec![format!("entry_path: {}", entry_path.display())],
            )
        })?;
    }
    let minified = minify_source(&bundled, false).map_err(|error| {
        error_with_context(
            "minify",
            format!("failed to minify bundled modules: {error}"),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
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

    let mut diagnostics = side_effect_diagnostics(side_effects_mode, &entry_path);
    diagnostics.extend(graph.diagnostics.iter().map(|diagnostic| ImportDiagnostic {
        stage: diagnostic.stage.clone(),
        message: diagnostic.message.clone(),
        details: diagnostic.details.clone(),
    }));
    diagnostics.extend(missing_export_diagnostics(request, &graph));

    Ok(ImportResult {
        specifier: request.specifier.clone(),
        raw_bytes: bundled.len() as u64,
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
            bundled.len() as u64,
        ),
        is_cjs: false,
        error: None,
        diagnostics,
    })
}

fn requested_exports(request: &ImportRequest) -> Vec<String> {
    match request.import_kind {
        ImportKind::Named => request.named.clone(),
        ImportKind::Default => vec!["default".to_owned()],
        ImportKind::Namespace | ImportKind::Dynamic => Vec::new(),
    }
}

fn is_truly_treeshakeable(
    request: &ImportRequest,
    side_effects: bool,
    graph: &crate::pipeline::graph::ModuleGraph,
    bundled_len: u64,
) -> bool {
    if side_effects || !matches!(request.import_kind, ImportKind::Named) {
        return false;
    }

    // To check if genuinely tree-shakeable, we compare against the full module size.
    let reachable_full = reachable_exports(graph, &[], true);
    let Ok(bundled_full) = bundle_reachable_modules(graph, &reachable_full) else {
        return false;
    };

    let full_len = bundled_full.len() as u64;
    if full_len == 0 {
        return false;
    }

    // If the tree-shaken size is within 5% of the full size, it's not truly tree-shakeable
    let ratio = (bundled_len as f64) / (full_len as f64);
    ratio <= 0.95
}

fn analyze_static_entry(
    context: &AnalysisContext,
    request: &ImportRequest,
    entry_path: PathBuf,
    side_effects_mode: SideEffectsMode,
    is_cjs: bool,
) -> Result<ImportResult, AnalysisError> {
    let side_effects = side_effects_mode.has_side_effects();
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
        truly_treeshakeable: !side_effects
            && !is_cjs
            && matches!(request.import_kind, ImportKind::Named),
        is_cjs,
        error: None,
        diagnostics: side_effect_diagnostics(side_effects_mode, &entry_path),
    })
}

fn side_effect_diagnostics(
    side_effects_mode: SideEffectsMode,
    entry_path: &Path,
) -> Vec<ImportDiagnostic> {
    if side_effects_mode != SideEffectsMode::Array {
        return Vec::new();
    }

    vec![ImportDiagnostic {
        stage: "side_effects".to_owned(),
        message: "package sideEffects array requires conservative full-graph analysis".to_owned(),
        details: vec![
            "sideEffects: array".to_owned(),
            format!("entry_path: {}", entry_path.display()),
        ],
    }]
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

fn missing_export_diagnostics(
    request: &ImportRequest,
    graph: &ModuleGraph,
) -> Vec<ImportDiagnostic> {
    if !matches!(request.import_kind, ImportKind::Named) {
        return Vec::new();
    }

    let missing = request
        .named
        .iter()
        .filter(|exported_name| {
            !graph_exports_name(graph, graph.entry_id, exported_name, &mut HashSet::new())
        })
        .cloned()
        .collect::<Vec<_>>();

    if missing.is_empty() {
        return Vec::new();
    }

    vec![ImportDiagnostic {
        stage: "exports".to_owned(),
        message: format!("named export(s) not found: {}", missing.join(", ")),
        details: vec![
            format!("specifier: {}", request.specifier),
            format!("missing_exports: {}", missing.join(", ")),
        ],
    }]
}

fn graph_exports_name(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> bool {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return false;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return false;
    };

    if module
        .exports
        .iter()
        .any(|export| export.exported_name == exported_name)
    {
        return true;
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        if reexport.imported_name == "*" {
            return true;
        }

        if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path)
            && graph_exports_name(graph, target_id, &reexport.imported_name, visited)
        {
            return true;
        }
    }

    for star_export in &module.star_exports {
        if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path)
            && graph_exports_name(graph, target_id, exported_name, visited)
        {
            return true;
        }
    }

    false
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
        error: Some(error.message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: error.stage.to_owned(),
            message: error.message,
            details: error.details,
        }],
    }
}

fn resolver_details(message: &str) -> Vec<String> {
    message
        .split("; ")
        .filter(|part| part.starts_with("checked:") || part.starts_with("candidate:"))
        .map(str::to_owned)
        .collect()
}

fn estimate_minified_source(source: &str) -> String {
    let mut stripped = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = None;

    while let Some(c) = chars.next() {
        if let Some(quote) = in_string {
            stripped.push(c);
            if c == '\\' {
                if let Some(escaped) = chars.next() {
                    stripped.push(escaped);
                }
            } else if c == quote {
                in_string = None;
            }
        } else {
            match c {
                '\'' | '"' | '`' => {
                    in_string = Some(c);
                    stripped.push(c);
                }
                '/' => {
                    if let Some(&next) = chars.peek() {
                        if next == '/' {
                            chars.next();
                            for comment_char in chars.by_ref() {
                                if comment_char == '\n' {
                                    stripped.push('\n');
                                    break;
                                }
                            }
                        } else if next == '*' {
                            chars.next();
                            let mut prev_star = false;
                            for comment_char in chars.by_ref() {
                                if prev_star && comment_char == '/' {
                                    break;
                                }
                                prev_star = comment_char == '*';
                            }
                        } else {
                            stripped.push(c);
                        }
                    } else {
                        stripped.push(c);
                    }
                }
                _ => stripped.push(c),
            }
        }
    }

    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
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
