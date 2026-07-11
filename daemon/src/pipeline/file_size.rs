use crate::{
    engine::{BundleEntry, BundlePurpose, BundleRequest, boundary},
    ipc::protocol::{
        ImportDiagnostic, ImportRequest, ImportResult, ImportRuntime, ModuleContribution,
    },
    pipeline::{
        analyze::{AnalysisContext, analyze_resolved_import, engine_selection},
        compress::compress_all,
        minify::minify_source,
        resolver::{ResolvedPackage, resolve_package_entry},
        util::diagnostic,
    },
};
use std::collections::{BTreeMap, HashMap};

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

/// Combined file sizing builds one multi-entry Rolldown bundle **per runtime** so
/// shared transitive modules are linked and counted once within a runtime.
///
/// A `BundleRequest` carries a single runtime, and Rolldown resolves the whole
/// transitive graph under it. Root entries are pre-resolved per request, so their
/// own paths are always right — but Server and Client resolve dependencies under
/// materially different conditions (`browser` alias fields, `browser` vs `node`
/// export conditions). Sizing every entry under one import's runtime therefore
/// resolves the *other* runtime's packages against the wrong conditions, and the
/// mis-conditioned build still succeeds, so nothing warns. A single Astro file
/// reaches this: frontmatter imports are Server, processed `<script>` imports are
/// Client (spec I15).
///
/// Grouping is per runtime rather than per entry on purpose: shared-module
/// deduplication is only ever real *within* a runtime, since Server and Client code
/// never share a chunk in the shipped product.
pub fn compute_file_size(
    context: &AnalysisContext,
    requests: &[ImportRequest],
) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    // Entries and their originating requests, grouped by the runtime they must be
    // built under. `BTreeMap` keeps the group order stable so identical input
    // produces identical output.
    let mut groups: BTreeMap<ImportRuntime, RuntimeGroup> = BTreeMap::new();

    for request in requests {
        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => {
                let group = groups.entry(request.runtime).or_default();
                group.entries.push(BundleEntry {
                    entry_path: resolved.entry_path.clone(),
                    package_root: resolved.package_root.clone(),
                    selection: engine_selection(request),
                    reported_side_effects: resolved.side_effects.clone(),
                });
                group.resolved_requests.push((request.clone(), resolved));
            }
            Err(error) => diagnostics.push(diagnostic(
                "entry_resolution",
                error,
                vec![format!("specifier: {}", request.specifier)],
            )),
        }
    }

    if groups.is_empty() {
        return FileSizeComputation {
            diagnostics,
            ..FileSizeComputation::default()
        };
    }

    let mut totals = FileSizeComputation::default();
    // Minified output of every group that built cleanly. Compression runs once over
    // the concatenation so a shared identifier across groups is not compressed twice
    // — which makes the compressed figures a lower bound on two independent bundles,
    // not a sum of them (recorded in the SRS).
    let mut minified_parts: Vec<String> = Vec::new();
    let mut any_sized = false;

    for (runtime, group) in groups {
        let artifact = match boundary::bundle_sync(BundleRequest {
            entries: group.entries,
            runtime,
            purpose: BundlePurpose::FileSize,
        }) {
            Ok(artifact) => artifact,
            Err(failure) => {
                // Only this runtime's entries degrade. The other groups keep their real,
                // shared-module-deduplicated numbers rather than being discarded with them.
                diagnostics.extend(failure.diagnostics.iter().map(|item| ImportDiagnostic {
                    stage: item.stage.clone(),
                    message: item.message.clone(),
                    details: Vec::new(),
                }));
                diagnostics.push(diagnostic(
                    &failure.stage,
                    failure.message,
                    vec![
                        "combined file-size build failed for this runtime; its totals are \
                         conservative per-import sums without shared-module deduplication"
                            .to_owned(),
                    ],
                ));

                let fallback =
                    per_import_totals(context, &group.resolved_requests, &mut diagnostics);
                if fallback.sized_any {
                    any_sized = true;
                    totals.raw_bytes += fallback.raw_bytes;
                    totals.minified_bytes += fallback.minified_bytes;
                    totals.gzip_bytes += fallback.gzip_bytes;
                    totals.brotli_bytes += fallback.brotli_bytes;
                    totals.zstd_bytes += fallback.zstd_bytes;
                }
                continue;
            }
        };

        // `record_loaded_paths` is deliberately NOT called here. This build's
        // `loaded_paths` is the union over every entry in the group, and writing that
        // union under each entry's key would clobber the accurate per-entry sets the
        // per-import analyses already recorded (`analyze.rs`), making an edit to one
        // package invalidate another document's cached size for an unrelated one
        // (spec I14).
        diagnostics.extend(artifact.diagnostics.iter().map(|item| ImportDiagnostic {
            stage: item.stage.clone(),
            message: item.message.clone(),
            details: Vec::new(),
        }));

        let minified = match minify_source(&artifact.code, false) {
            Ok(minified) => minified,
            Err(error) => return error_computation("minify", error, diagnostics),
        };

        any_sized = true;
        totals.raw_bytes += artifact.code.len() as u64;
        totals.minified_bytes += minified.len() as u64;
        minified_parts.push(minified);
    }

    if !any_sized {
        return error_computation(
            "file_size_fallback",
            "no import could be sized conservatively".to_owned(),
            diagnostics,
        );
    }

    if !minified_parts.is_empty() {
        let minified = minified_parts.join("\n");
        let compressed = match compress_all(&minified) {
            Ok(compressed) => compressed,
            Err(error) => return error_computation("compression", error.to_string(), diagnostics),
        };
        totals.gzip_bytes += compressed.gzip_bytes;
        totals.brotli_bytes += compressed.brotli_bytes;
        totals.zstd_bytes += compressed.zstd_bytes;
    }

    FileSizeComputation {
        diagnostics,
        error: None,
        ..totals
    }
}

#[derive(Default)]
struct RuntimeGroup {
    entries: Vec<BundleEntry>,
    resolved_requests: Vec<(ImportRequest, ResolvedPackage)>,
}

#[derive(Default)]
struct PerImportTotals {
    sized_any: bool,
    raw_bytes: u64,
    minified_bytes: u64,
    gzip_bytes: u64,
    brotli_bytes: u64,
    zstd_bytes: u64,
}

/// A file-level request must degrade to conservative non-deduped per-import totals
/// instead of zeroing the aggregate when a package breaks the combined build (SRS
/// FR-024a). Each per-import analysis applies its own static fallback on engine
/// failure, so only imports that cannot be sized at all are dropped from the sum.
///
/// Applied per runtime group, so a failure under one runtime never discards the
/// other's real, deduplicated numbers.
fn per_import_totals(
    context: &AnalysisContext,
    resolved_requests: &[(ImportRequest, ResolvedPackage)],
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> PerImportTotals {
    let mut totals = PerImportTotals::default();

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
        totals.sized_any = true;
        totals.raw_bytes += result.raw_bytes;
        totals.minified_bytes += result.minified_bytes;
        totals.gzip_bytes += result.gzip_bytes;
        totals.brotli_bytes += result.brotli_bytes;
        totals.zstd_bytes += result.zstd_bytes;
    }

    totals
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
