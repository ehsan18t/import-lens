use crate::{
    ipc::protocol::{
        ImportDiagnostic, ImportKind, ImportRequest, ImportResult, ModuleContribution,
    },
    pipeline::{
        analyze::AnalysisContext,
        bundle::bundle_reachable_modules_with_metadata,
        cjs::analyze_cjs_graph_with_runtime,
        compress::compress_all,
        graph::{ModuleGraph, ModuleId, ModuleRecord, build_module_graph_cached_with_runtime},
        minify::minify_source_with_markers,
        reachability::{ReachableExports, reachable_exports},
        resolver::resolve_package_entry,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Debug, Default)]
pub struct FileSizeComputation {
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

pub fn annotate_shared_bytes(results: &mut [ImportResult]) {
    let mut counts = HashMap::<String, usize>::new();

    for module in results
        .iter()
        .flat_map(|result| result_contributions(result).iter())
    {
        *counts.entry(module.path.clone()).or_default() += 1;
    }

    for result in results {
        let shared = result_contributions(result)
            .iter()
            .filter(|module| counts.get(&module.path).copied().unwrap_or_default() > 1)
            .map(|module| module.bytes)
            .sum();

        result.shared_bytes = Some(shared);
    }
}

fn result_contributions(result: &ImportResult) -> &[ModuleContribution] {
    if result.internal_contributions.is_empty() {
        return result.module_breakdown.as_deref().unwrap_or_default();
    }

    &result.internal_contributions
}

pub fn compute_file_size(
    context: &AnalysisContext,
    requests: &[ImportRequest],
) -> FileSizeComputation {
    let mut combined_modules = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut combined_reachable = ReachableExports::default();
    let mut conservative_sources = Vec::new();
    let mut diagnostics = Vec::new();

    for request in requests {
        let resolved = match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => resolved,
            Err(error) => {
                diagnostics.push(diagnostic(
                    "entry_resolution",
                    error,
                    vec![format!("specifier: {}", request.specifier)],
                ));
                continue;
            }
        };

        if resolved.is_cjs {
            diagnostics.push(diagnostic(
                "file_size",
                "CommonJS import included conservatively without file-level deduplication"
                    .to_owned(),
                vec![format!("specifier: {}", request.specifier)],
            ));
            match analyze_cjs_graph_with_runtime(&resolved.entry_path, request.runtime) {
                Ok(graph) => {
                    diagnostics.extend(graph.diagnostics);
                    if graph.unsupported {
                        diagnostics.push(diagnostic(
                            "file_size",
                            "CommonJS graph contains unsupported dynamic require; included known files conservatively".to_owned(),
                            vec![format!("specifier: {}", request.specifier)],
                        ));
                    }
                    conservative_sources.push(graph.source);
                }
                Err(error) => {
                    diagnostics.push(diagnostic(
                        "file_size",
                        format!(
                            "CommonJS graph failed; included entry file conservatively: {error}"
                        ),
                        vec![format!("entry_path: {}", resolved.entry_path.display())],
                    ));
                    match std::fs::read_to_string(&resolved.entry_path) {
                        Ok(source) => {
                            conservative_sources.push(format!(";(() => {{\n{source}\n}})();"))
                        }
                        Err(read_error) => diagnostics.push(diagnostic(
                            "file_size",
                            format!(
                                "failed to read CommonJS entry for file-size fallback: {read_error}"
                            ),
                            vec![format!("entry_path: {}", resolved.entry_path.display())],
                        )),
                    }
                }
            }
            continue;
        }

        let graph =
            match build_module_graph_cached_with_runtime(&resolved.entry_path, request.runtime) {
                Ok(graph) => graph,
                Err(error) => {
                    diagnostics.push(diagnostic(
                        "module_graph",
                        error,
                        vec![format!("entry_path: {}", resolved.entry_path.display())],
                    ));
                    continue;
                }
            };
        diagnostics.extend(graph.diagnostics.iter().map(|item| ImportDiagnostic {
            stage: item.stage.clone(),
            message: item.message.clone(),
            details: item.details.clone(),
        }));

        let include_full_entry = resolved.side_effects.has_side_effects()
            || matches!(
                request.import_kind,
                ImportKind::Namespace | ImportKind::Dynamic
            );
        let reachable = reachable_exports(&graph, &requested_exports(request), include_full_entry);
        combined_reachable.merge_from(&reachable);
        merge_graph_modules(&mut combined_modules, &mut seen_paths, &graph);
    }

    if combined_modules.is_empty() && conservative_sources.is_empty() {
        return FileSizeComputation {
            diagnostics,
            ..FileSizeComputation::default()
        };
    }

    let mut source = String::new();
    let mut minifier_source = String::new();

    if !combined_modules.is_empty() {
        let combined_graph = ModuleGraph::from_parts(
            ModuleId(0),
            combined_modules,
            Vec::new(),
            seen_paths.into_iter().collect(),
        );
        let bundled =
            match bundle_reachable_modules_with_metadata(&combined_graph, &combined_reachable) {
                Ok(bundled) => bundled,
                Err(error) => return error_computation("bundle", error, diagnostics),
            };
        source.push_str(&bundled.source);
        minifier_source.push_str(&bundled.minifier_source);
    }

    for conservative_source in conservative_sources {
        source.push_str(&conservative_source);
        source.push('\n');
        minifier_source.push_str(&conservative_source);
        minifier_source.push('\n');
    }

    let minified = match minify_source_with_markers(&minifier_source, false) {
        Ok(minified) => minified,
        Err(error) => return error_computation("minify", error, diagnostics),
    };
    let compressed = match compress_all(&minified) {
        Ok(compressed) => compressed,
        Err(error) => return error_computation("compression", error.to_string(), diagnostics),
    };

    FileSizeComputation {
        raw_bytes: source.len() as u64,
        minified_bytes: minified.len() as u64,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        error: None,
        diagnostics,
    }
}

fn requested_exports(request: &ImportRequest) -> Vec<String> {
    match request.import_kind {
        ImportKind::Named => request.named.clone(),
        ImportKind::Default => vec!["default".to_owned()],
        ImportKind::Namespace | ImportKind::Dynamic => Vec::new(),
    }
}

fn merge_graph_modules(
    modules: &mut Vec<ModuleRecord>,
    seen_paths: &mut HashSet<PathBuf>,
    graph: &ModuleGraph,
) {
    for module in &graph.modules {
        if !seen_paths.insert(module.path.clone()) {
            continue;
        }

        let mut module = module.clone();
        module.id = ModuleId(modules.len());
        modules.push(module);
    }
}

fn error_computation(
    stage: &str,
    message: String,
    mut diagnostics: Vec<ImportDiagnostic>,
) -> FileSizeComputation {
    diagnostics.push(diagnostic(stage, message.clone(), Vec::new()));

    FileSizeComputation {
        error: Some(message),
        diagnostics,
        ..FileSizeComputation::default()
    }
}

fn diagnostic(stage: &str, message: String, details: Vec<String>) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: stage.to_owned(),
        message,
        details,
    }
}
