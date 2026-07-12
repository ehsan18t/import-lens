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
/// the fallback sums measurements, and there is not one yet. A fallback that had to skip it is
/// marked [`FileSizeComputation::incomplete`] and is never cached.
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
    /// At least one import that belongs in these totals was not really measured: its own build had
    /// not landed when the sum was taken, or the measurement it did have was fabricated by a
    /// transient engine failure. The totals are then a LOWER BOUND on the file, not the file — safe
    /// to show beside the diagnostics that say so (FR-024a: conservative totals, never zero), and
    /// never safe to cache.
    ///
    /// It exists because neither of the other two signals can see this. `error` is `None` — the sum
    /// succeeded, it just summed less than the file. And the stage scan below sees only the
    /// diagnostics the aggregate carries, which for a still-building import are not a failure of
    /// any stage at all.
    pub incomplete: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

impl FileSizeComputation {
    /// Whether this aggregate is a measurement of the file, and so may be written to the L1
    /// file-size cache (SRS FR-026c).
    ///
    /// Three ways it is not. It failed outright (`error`). It was degraded by a **transient**
    /// engine failure — a combined build that timed out, panicked, or lost the runtime — which
    /// describes *this run of the daemon*, not the file; caching it would serve that number for the
    /// whole 30s TTL of a document whose real total is larger, the same defect that let one parked
    /// build teach the import cache that a healthy package weighs 58 bytes. Or it is `incomplete`:
    /// a conservative sum that is missing an input, which is a real number but not this file's.
    ///
    /// A DETERMINISTIC failure (`minify`, `entry_resolution`, an unresolvable import) is a property
    /// of the code and is cached as before: re-running it changes nothing.
    pub fn is_cacheable(&self) -> bool {
        self.error.is_none()
            && !self.incomplete
            && !self
                .diagnostics
                .iter()
                .any(|item| crate::engine::stage::is_transient(&item.stage))
    }

    /// Fold one runtime group's conservative per-import sum into the file's totals, and report
    /// whether it contributed anything.
    ///
    /// The ONLY way a fallback sum reaches the totals, which is the point: the "an input was not
    /// really measured" flag travels WITH the bytes and is applied here, so a caller cannot add the
    /// bytes and forget the flag. It has been forgotten three times in this design — a circuit
    /// breaker that condemned a healthy package, a degraded import result cached over a healthy
    /// one, and this file total — always because a fabricated number and a measured one are the
    /// same `u64`.
    fn absorb_fallback(&mut self, fallback: PerImportTotals) -> bool {
        self.incomplete |= fallback.missing_inputs;
        if !fallback.sized_any {
            return false;
        }

        self.raw_bytes += fallback.raw_bytes;
        self.minified_bytes += fallback.minified_bytes;
        self.gzip_bytes += fallback.gzip_bytes;
        self.brotli_bytes += fallback.brotli_bytes;
        self.zstd_bytes += fallback.zstd_bytes;
        true
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
                any_sized |= totals.absorb_fallback(fallback);
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
                any_sized |= totals.absorb_fallback(fallback);
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

/// A runtime group's conservative sum, plus the one fact the bytes alone cannot carry: whether
/// every import that belongs in it was really measured.
///
/// Only [`FileSizeComputation::absorb_fallback`] consumes this, and it applies both halves at once,
/// so the sum cannot silently swallow a missing input.
#[derive(Default)]
struct PerImportTotals {
    sized_any: bool,
    /// An import contributed no real measurement of its own: it is still being built (`result:
    /// None`), or a transient engine failure fabricated the number it does carry. Either way this
    /// sum is under the file's true size, or made of numbers that describe a scheduling accident.
    missing_inputs: bool,
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
/// Three imports make the sum something other than the file's size, and every one of them is named
/// in the diagnostics, because the user is owed the fact that the number is a floor:
///
/// * one whose own build has **not landed yet** (`result: None` — the streaming handlers answer
///   from cache and let the misses arrive later). It contributes zero, so the sum is short by
///   exactly its weight, and the group could even report zero bytes with `error: None`;
/// * one a **transient engine failure** degraded. That result carries `error: None` and a
///   fabricated static size, and the failure is recorded only in the result's OWN diagnostics — so
///   without lifting it here the fabricated number would land in the file's total and be cached as
///   the file's size. This is the 58-byte cache-poisoning defect, one level up. Its bytes are still
///   counted (a floor beats a zero, FR-024a) but the total is marked and never cached;
/// * one that failed to size at all (`error`). That one is *deterministic* — the same request will
///   fail the same way — so it contributes zero and does NOT taint the total: it is cached exactly
///   as `should_cache_result` caches a deterministic per-import failure.
fn per_import_totals(
    sized: &[SizedImport],
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> PerImportTotals {
    let mut totals = PerImportTotals::default();

    for import in sized {
        let specifier = format!("specifier: {}", import.request.specifier);
        let Some(result) = import.result.as_ref() else {
            totals.missing_inputs = true;
            diagnostics.push(diagnostic(
                "file_size_fallback",
                "import size is still being measured, so it is not counted in this file's \
                 conservative total"
                    .to_owned(),
                vec![specifier],
            ));
            continue;
        };
        if let Some(error) = result.error.as_ref() {
            diagnostics.push(diagnostic(
                "file_size_fallback",
                error.clone(),
                vec![specifier],
            ));
            continue;
        }

        for degraded in result
            .diagnostics
            .iter()
            .filter(|item| crate::engine::stage::is_transient(&item.stage))
        {
            totals.missing_inputs = true;
            diagnostics.push(diagnostic(
                &degraded.stage,
                degraded.message.clone(),
                vec![
                    specifier.clone(),
                    "this import's own build failed transiently, so its size here is the static \
                     fallback and the file's total is an estimate"
                        .to_owned(),
                ],
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::stage;
    use crate::ipc::protocol::{ConfidenceLevel, ImportKind, ResultFreshness};

    fn request(specifier: &str) -> ImportRequest {
        ImportRequest {
            specifier: specifier.to_owned(),
            package_name: specifier.to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Namespace,
            runtime: ImportRuntime::Component,
        }
    }

    fn result(specifier: &str, bytes: u64) -> ImportResult {
        ImportResult {
            specifier: specifier.to_owned(),
            raw_bytes: bytes,
            minified_bytes: bytes,
            gzip_bytes: bytes,
            brotli_bytes: bytes,
            zstd_bytes: bytes,
            cache_hit: false,
            side_effects: false,
            truly_treeshakeable: true,
            is_cjs: false,
            confidence: ConfidenceLevel::High,
            confidence_reasons: Vec::new(),
            error: None,
            diagnostics: Vec::new(),
            module_breakdown: None,
            shared_bytes: None,
            freshness: ResultFreshness::fresh(),
            internal_contributions: Vec::new(),
        }
    }

    /// The shape a TIMEOUT/PANIC leaves behind: `error: None`, a plausible byte count that is
    /// actually the static entry fallback, and the failure recorded only in the result's own
    /// diagnostics.
    fn transiently_degraded(specifier: &str, bytes: u64) -> ImportResult {
        ImportResult {
            diagnostics: vec![ImportDiagnostic::for_stage(
                stage::TIMEOUT,
                "engine build did not complete within 8s",
            )],
            ..result(specifier, bytes)
        }
    }

    fn measured(specifier: &str, bytes: u64) -> SizedImport {
        SizedImport {
            request: request(specifier),
            result: Some(result(specifier, bytes)),
        }
    }

    fn absorb(sized: &[SizedImport]) -> FileSizeComputation {
        let mut diagnostics = Vec::new();
        let fallback = per_import_totals(sized, &mut diagnostics);
        let mut totals = FileSizeComputation::default();
        totals.absorb_fallback(fallback);
        totals.diagnostics = diagnostics;
        totals
    }

    #[test]
    fn a_sum_of_real_measurements_is_the_file_and_is_cacheable() {
        let totals = absorb(&[measured("alpha", 100), measured("beta", 20)]);

        assert_eq!(totals.raw_bytes, 120);
        assert!(!totals.incomplete);
        assert!(
            totals.is_cacheable(),
            "every import was really measured, so the sum IS this file's size"
        );
    }

    /// The streaming handlers answer a cold import `loading`, so its `result` is `None` when the
    /// combined build fails and the conservative sum is taken. It contributes exactly zero, and a
    /// total that is short by one whole import must never be served as the file's size for the L1
    /// TTL.
    #[test]
    fn a_sum_missing_a_still_building_import_is_not_the_file_and_is_never_cached() {
        let totals = absorb(&[
            measured("alpha", 100),
            SizedImport {
                request: request("beta"),
                result: None,
            },
        ]);

        assert_eq!(totals.raw_bytes, 100, "the missing import contributes zero");
        assert!(totals.incomplete);
        assert!(
            !totals.is_cacheable(),
            "a total that is missing an input is a lower bound, not a measurement"
        );
        assert!(
            totals.diagnostics.iter().any(|item| item
                .details
                .iter()
                .any(|detail| detail == "specifier: beta")),
            "the user is owed the fact that the number is a floor: {:?}",
            totals.diagnostics
        );
    }

    /// The 58-byte defect, one level up. A build that timed out or panicked degrades to a static
    /// entry size that carries `error: None` — so the sum happily adds it, and nothing in the
    /// aggregate's own diagnostics says where the number came from. Lift the transient stage out of
    /// the result, or the fabricated total is written to the process-wide L1 cache.
    #[test]
    fn a_sum_of_a_transiently_degraded_import_carries_its_stage_and_is_never_cached() {
        let mut diagnostics = Vec::new();
        let fallback = per_import_totals(
            &[
                measured("alpha", 100),
                SizedImport {
                    request: request("beta"),
                    result: Some(transiently_degraded("beta", 58)),
                },
            ],
            &mut diagnostics,
        );
        let mut totals = FileSizeComputation::default();
        totals.absorb_fallback(fallback);
        totals.diagnostics = diagnostics;

        assert_eq!(
            totals.raw_bytes, 158,
            "a floor beats a zero (FR-024a): the fabricated size is still counted"
        );
        assert!(totals.incomplete);
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|item| stage::is_transient(&item.stage)),
            "the import's transient failure must reach the aggregate: {:?}",
            totals.diagnostics
        );
        assert!(
            !totals.is_cacheable(),
            "a total built on a fabricated size describes this run of the daemon, not the file"
        );
    }

    /// The correction must not over-reach. An import that failed DETERMINISTICALLY has no size, and
    /// re-running it would learn the same thing at the cost of another build — so it contributes
    /// zero and the total is still cached, exactly as `should_cache_result` caches the per-import
    /// failure itself.
    #[test]
    fn a_deterministically_failed_import_contributes_zero_and_still_caches() {
        let failed = ImportResult {
            error: Some("no matching export".to_owned()),
            ..result("beta", 0)
        };
        let totals = absorb(&[
            measured("alpha", 100),
            SizedImport {
                request: request("beta"),
                result: Some(failed),
            },
        ]);

        assert_eq!(totals.raw_bytes, 100);
        assert!(!totals.incomplete);
        assert!(totals.is_cacheable());
    }
}
