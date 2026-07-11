use crate::{
    engine::{
        BundleEntry, BundlePurpose, BundleRequest, boundary, dependency_paths::record_loaded_paths,
    },
    ipc::protocol::{ImportDiagnostic, ImportRequest, ImportResult, ModuleContribution},
    pipeline::{
        analyze::{AnalysisContext, analyze_resolved_import, engine_selection},
        compress::compress_all,
        minify::minify_source,
        resolver::{ResolvedPackage, resolve_package_entry},
        util::diagnostic,
    },
};
use std::collections::HashMap;

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

/// Combined file sizing uses one multi-entry Rolldown build so shared
/// transitive modules are linked and counted once.
pub fn compute_file_size(
    context: &AnalysisContext,
    requests: &[ImportRequest],
) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    let mut entries = Vec::new();
    let mut entry_keys = Vec::new();
    let mut resolved_requests = Vec::new();
    let mut runtime = None;

    for request in requests {
        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => {
                runtime.get_or_insert(request.runtime);
                entry_keys.push((resolved.entry_path.clone(), request.runtime));
                entries.push(BundleEntry {
                    entry_path: resolved.entry_path.clone(),
                    package_root: resolved.package_root.clone(),
                    selection: engine_selection(request),
                    reported_side_effects: resolved.side_effects.clone(),
                });
                resolved_requests.push((request.clone(), resolved));
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
            diagnostics.push(diagnostic(
                &failure.stage,
                failure.message,
                vec![
                    "combined file-size build failed; totals are conservative per-import sums \
                     without shared-module deduplication"
                        .to_owned(),
                ],
            ));
            return per_import_fallback_totals(context, &resolved_requests, diagnostics);
        }
    };

    for (entry_path, runtime) in entry_keys {
        record_loaded_paths(entry_path, runtime, artifact.loaded_paths.clone());
    }
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

/// A file-level request must degrade to conservative non-deduped per-import
/// totals instead of zeroing the whole aggregate when one package breaks the
/// combined build (SRS FR-024a). Each per-import analysis applies its own
/// static fallback on engine failure, so only imports that cannot be sized at
/// all are dropped from the sum.
fn per_import_fallback_totals(
    context: &AnalysisContext,
    resolved_requests: &[(ImportRequest, ResolvedPackage)],
    mut diagnostics: Vec<ImportDiagnostic>,
) -> FileSizeComputation {
    let mut totals = FileSizeComputation::default();
    let mut any_sized = false;

    for (request, resolved) in resolved_requests {
        let result = analyze_resolved_import(context, request, resolved.clone());
        if let Some(error) = result.error {
            diagnostics.push(diagnostic(
                "file_size_fallback",
                error,
                vec![format!("specifier: {}", request.specifier)],
            ));
            continue;
        }
        any_sized = true;
        totals.raw_bytes += result.raw_bytes;
        totals.minified_bytes += result.minified_bytes;
        totals.gzip_bytes += result.gzip_bytes;
        totals.brotli_bytes += result.brotli_bytes;
        totals.zstd_bytes += result.zstd_bytes;
    }

    if !any_sized {
        return error_computation(
            "file_size_fallback",
            "no import could be sized conservatively".to_owned(),
            diagnostics,
        );
    }

    FileSizeComputation {
        diagnostics,
        ..totals
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
