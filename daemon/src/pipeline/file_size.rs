use crate::{
    ipc::protocol::{
        ImportDiagnostic, ImportKind, ImportRequest, ImportResult, ModuleContribution,
    },
    pipeline::{
        analyze::{AnalysisContext, side_effect_diagnostics},
        bundle::bundle_reachable_modules_with_metadata,
        cjs::analyze_cjs_graph_with_runtime,
        compress::compress_all,
        graph::{ModuleGraph, ModuleId, ModuleRecord, build_module_graph_cached_with_runtime},
        minify::minify_source_with_markers,
        reachability::{ReachableExports, reachable_exports, requested_exports},
        resolver::resolve_package_entry,
        util::diagnostic,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Debug, Default, Clone)]
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
    // Phase 2 selection seam (spec §6.3): one multi-entry engine request —
    // never per-package concatenation — once the Phase 3 cutover flips
    // `USE_ROLLDOWN_ENGINE`.
    if crate::engine::USE_ROLLDOWN_ENGINE {
        return compute_file_size_with_engine(context, requests);
    }

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

        let side_effect_matches = resolved
            .side_effects
            .matching_paths(graph.modules.iter().map(|module| module.path.as_path()));
        diagnostics.extend(side_effect_diagnostics(
            &resolved.side_effects,
            &resolved.entry_path,
            &side_effect_matches,
        ));

        let include_full_entry = resolved.side_effects.has_side_effects()
            || matches!(
                request.import_kind,
                ImportKind::Namespace | ImportKind::Dynamic
            );
        let mut reachable =
            reachable_exports(&graph, &requested_exports(request), include_full_entry);
        for path in side_effect_matches {
            reachable.mark_full_module(path);
        }
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

/// Rolldown-backed combined sizing (spec §6.3): every resolvable import
/// becomes one entry of a single multi-entry build, so shared transitive
/// modules are counted once by the linker itself. Public because the Phase 2
/// differential tests drive it directly while `USE_ROLLDOWN_ENGINE` keeps
/// production on the legacy pipeline.
pub fn compute_file_size_with_engine(
    context: &AnalysisContext,
    requests: &[ImportRequest],
) -> FileSizeComputation {
    use crate::engine::{BundleEntry, BundlePurpose, BundleRequest, boundary};
    use crate::pipeline::analyze::engine_selection;
    use crate::pipeline::minify::minify_source;

    let mut diagnostics = Vec::new();
    let mut entries = Vec::new();
    // Imports inside one document share a runtime in practice; the first
    // resolvable request's runtime conditions the whole combined build.
    let mut runtime = None;
    for request in requests {
        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => {
                runtime.get_or_insert(request.runtime);
                entries.push(BundleEntry {
                    entry_path: resolved.entry_path,
                    package_root: resolved.package_root,
                    selection: engine_selection(request),
                    reported_side_effects: resolved.side_effects,
                });
            }
            Err(error) => diagnostics.push(diagnostic(
                "entry_resolution",
                error,
                vec![format!("specifier: {}", request.specifier)],
            )),
        }
    }
    if entries.is_empty() {
        return FileSizeComputation {
            diagnostics,
            ..FileSizeComputation::default()
        };
    }

    let artifact = match boundary::bundle_sync(BundleRequest {
        entries,
        runtime: runtime.unwrap_or_default(),
        purpose: BundlePurpose::FileSize,
    }) {
        Ok(artifact) => artifact,
        Err(failure) => {
            diagnostics.extend(failure.diagnostics.iter().map(|item| ImportDiagnostic {
                stage: item.stage.clone(),
                message: item.message.clone(),
                details: Vec::new(),
            }));
            let stage = failure.stage.clone();
            return error_computation(&stage, failure.message, diagnostics);
        }
    };
    diagnostics.extend(artifact.diagnostics.iter().map(|item| ImportDiagnostic {
        stage: item.stage.clone(),
        message: item.message.clone(),
        details: Vec::new(),
    }));

    let minified = match minify_source(&artifact.code, false) {
        Ok(minified) => minified,
        Err(error) => return error_computation("minify", error, diagnostics),
    };
    let compressed = match compress_all(&minified) {
        Ok(compressed) => compressed,
        Err(error) => return error_computation("compression", error.to_string(), diagnostics),
    };

    FileSizeComputation {
        raw_bytes: artifact.code.len() as u64,
        minified_bytes: minified.len() as u64,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        error: None,
        diagnostics,
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
