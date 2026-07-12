use crate::{
    engine::{BundleEntry, BundlePurpose, BundleRequest, boundary},
    ipc::protocol::{
        ImportDiagnostic, ImportRequest, ImportResult, ImportRuntime, ModuleContribution,
    },
    pipeline::{
        analyze::{AnalysisContext, engine_selection},
        compress::compress_all,
        minify::minify_source,
        resolver::resolve_package_entry,
        util::diagnostic,
    },
};
use std::collections::{BTreeMap, HashMap};

/// One import a file-size computation must account for, together with whatever the caller has
/// already measured for it.
///
/// `result` is `None` while that import's own build is still in flight — the streaming document
/// handlers answer from cache and let the misses land later (`ipc::server`). Such an import is
/// still an *entry* of the combined build (its bytes belong in the file's total), but it can
/// contribute nothing to the conservative per-import fallback below, which is the honest thing:
/// the fallback sums measurements, and there is not one yet.
///
/// Carrying the measurement in rather than re-deriving it is also what keeps the fallback out of
/// the engine. It used to re-analyze every import of the failing runtime group from scratch, so
/// one combined build that parked cost a build timeout and then N more — duplicating, on a second
/// set of permits, the very builds the caller had already run or was already running.
#[derive(Debug, Clone)]
pub struct SizedImport {
    pub request: ImportRequest,
    pub result: Option<ImportResult>,
}

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

impl FileSizeComputation {
    /// Whether this aggregate is a measurement, and so may be written to the L1 file-size cache.
    ///
    /// A combined build that timed out, panicked, or lost the engine runtime degrades the file's
    /// totals to a conservative per-import sum — a number that describes *this run of the
    /// daemon*, not the file. Caching it would serve that number for the whole TTL of a document
    /// whose real total is larger, which is the same defect that let one parked build teach the
    /// import cache that a healthy package weighs 58 bytes. A deterministic failure
    /// (`minify`, `entry_resolution`, an unresolvable import) is a property of the code and is
    /// cached as before: re-running it changes nothing.
    pub fn is_cacheable(&self) -> bool {
        self.error.is_none()
            && !self
                .diagnostics
                .iter()
                .any(|item| crate::engine::stage::is_transient(&item.stage))
    }
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
    imports: &[SizedImport],
) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    // Entries and their originating imports, grouped by the runtime they must be
    // built under. `BTreeMap` keeps the group order stable so identical input
    // produces identical output.
    let mut groups: BTreeMap<ImportRuntime, RuntimeGroup> = BTreeMap::new();

    for import in imports {
        let request = &import.request;
        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => {
                let group = groups.entry(request.runtime).or_default();
                group.entries.push(BundleEntry {
                    entry_path: resolved.entry_path.clone(),
                    package_root: resolved.package_root.clone(),
                    selection: engine_selection(request),
                    reported_side_effects: resolved.side_effects.clone(),
                });
                group.sized.push(import.clone());
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

                let fallback = per_import_totals(&group.sized, &mut diagnostics);
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
            Err(error) => {
                // Degrade only this runtime, exactly as a build failure does. Returning
                // here would discard every other group's real totals and report zero
                // for the whole file.
                diagnostics.push(diagnostic(
                    "minify",
                    error,
                    vec![
                        "minification failed for this runtime; its totals are conservative \
                         per-import sums without shared-module deduplication"
                            .to_owned(),
                    ],
                ));
                let fallback = per_import_totals(&group.sized, &mut diagnostics);
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

        any_sized = true;
        totals.raw_bytes += artifact.code.len() as u64;
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
        // Measure `minified_bytes` on the same string the compressors saw, so the two
        // numbers describe the same bytes (the join adds one separator per extra
        // group).
        totals.minified_bytes += minified.len() as u64;
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
    sized: Vec<SizedImport>,
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
/// FR-024a). Applied per runtime group, so a failure under one runtime never discards
/// the other's real, deduplicated numbers.
///
/// It sums the measurements the caller already has, and **never enters the engine**. It used to
/// re-analyze each import from scratch here, which is how one combined build that parked turned
/// into a build timeout plus one more per import — the tail that the request budget existed to
/// cut off, at the cost of fabricating the numbers it cut. Nothing here can park, so nothing
/// needs cutting off.
///
/// Two imports contribute nothing: one that failed to size at all (`error`), and one whose own
/// build has not landed yet (`result: None` — the streaming handlers answer from cache). Both
/// are named in the diagnostics, because the total is then a lower bound and the user is owed
/// that fact.
fn per_import_totals(
    sized: &[SizedImport],
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> PerImportTotals {
    let mut totals = PerImportTotals::default();

    for import in sized {
        let Some(result) = import.result.as_ref() else {
            diagnostics.push(diagnostic(
                "file_size_fallback",
                "import size is still being measured, so it is not counted in this file's \
                 conservative total"
                    .to_owned(),
                vec![format!("specifier: {}", import.request.specifier)],
            ));
            continue;
        };
        if let Some(error) = result.error.as_ref() {
            diagnostics.push(diagnostic(
                "file_size_fallback",
                error.clone(),
                vec![format!("specifier: {}", import.request.specifier)],
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
