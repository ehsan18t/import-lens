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
    /// At least one import that belongs in these totals contributed **no bytes**, because it was
    /// not Measured. The totals are then a LOWER BOUND on the file, not the file — safe to show
    /// beside the diagnostics that say so (FR-024a: a floor beats a zero), and never safe to cache,
    /// persist, or compare against a baseline (ADR-0006, invariant 4).
    ///
    /// **Any** non-Measured contributor sets it. All three kinds:
    ///
    /// * **Loading** — its own build had not landed when the sum was taken (`result: None`).
    /// * **Unmeasured, transient** — timeout / panic / engine_gone. Says nothing about the package.
    /// * **Unmeasured, deterministic** — parse / link / missing_export / … Says a great deal about
    ///   the package, and *nothing at all about how many bytes it contributes*, which is the only
    ///   question a total asks. This one was exempted, and the exemption is the seventh instance of
    ///   the defect this model exists to end: a deterministic failure also KILLS the file's combined
    ///   build, so the total collapses into an un-deduplicated per-import sum — a different number
    ///   for every import that *was* measured — and with `incomplete: false` that number was cached,
    ///   persisted as the file's permanent baseline, shown without an estimate label, and passed by
    ///   `importlens check` with exit 0. "Deterministically unknown" is still unknown.
    /// * and an import that could not be RESOLVED, which is not even an entry of the combined build,
    ///   so its bytes are absent from these totals however well that build went.
    ///
    /// It exists because neither of the other two signals can see this. `error` is `None` — the sum
    /// succeeded, it just summed less than the file. And the stage scan in [`Self::is_cacheable`]
    /// sees only transient stages, while a still-building import has failed at no stage at all.
    pub incomplete: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
}

impl FileSizeComputation {
    /// Whether this aggregate is a measurement of the file, and so may be written to the L1
    /// file-size cache (SRS FR-026c). [`crate::pipeline::file_size_cache::FileSizeCache::insert`]
    /// asks this itself; a caller cannot forget it.
    ///
    /// Three ways it is not. It failed outright (`error`). It is [`Self::incomplete`] — a sum
    /// missing an input, which is a real number but not this file's. Or a **transient** engine
    /// failure degraded the combined build itself (timeout / panic / engine_gone) and the sum it
    /// fell back to has no shared-module deduplication: caching that would serve a number the file
    /// never had for the whole 30s TTL, on the strength of a scheduling accident.
    ///
    /// The third check is not redundant with the second. A combined build can park while every one
    /// of the file's imports is measured and cached — `incomplete` is then correctly `false`, and
    /// the totals are still not the file's.
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
    let mut totals = FileSizeComputation::default();
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
            Err(error) => {
                // This import is not an ENTRY of any group, so its bytes are missing from the
                // totals however cleanly the combined builds go — the one non-Measured contributor
                // a successful build cannot absorb. Floor (ADR-0006, invariant 4).
                totals.incomplete = true;
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::ENTRY_RESOLUTION,
                    error,
                    vec![format!("specifier: {}", request.specifier)],
                ));
            }
        }
    }

    if groups.is_empty() {
        // Either the file has no imports (a complete, honest zero) or not one of them could be
        // resolved (`incomplete`, and never cached as this file's size).
        return FileSizeComputation {
            diagnostics,
            ..totals
        };
    }

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
                    crate::pipeline::stage::MINIFY,
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
            crate::pipeline::stage::FILE_SIZE_FALLBACK,
            "no import could be sized conservatively".to_owned(),
            diagnostics,
        );
    }

    if !minified_parts.is_empty() {
        let minified = minified_parts.join("\n");
        let compressed = match compress_all(&minified) {
            Ok(compressed) => compressed,
            Err(error) => {
                return error_computation(
                    crate::pipeline::stage::COMPRESSION,
                    error.to_string(),
                    diagnostics,
                );
            }
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
    /// An import that belongs in this sum contributed no bytes, because it was not Measured — it is
    /// still being built (`result: None`), or its build failed, transiently or otherwise. This sum
    /// is then under the file's true size by an amount the sum itself cannot know.
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
/// Only a **measured** import contributes bytes (ADR-0006: a size exists if and only if a build
/// succeeded). Every other kind contributes exactly zero, and **every one of them therefore makes
/// the sum a floor** — `missing_inputs`. That is invariant 4, stated without an exception, because
/// the exception is where the seventh instance of this defect lived:
///
/// * **Loading** (`result: None` — the streaming handlers answer from cache and let the misses
///   arrive later). The sum is short by exactly that import's weight.
/// * **Unmeasured, transient** (`timeout` / `panic` / `engine_gone`). Its bytes are unknown *for
///   this run only*; the very next attempt may measure it.
/// * **Unmeasured, deterministic** (`parse`, `link`, `missing_export`, `oversized_entry`, …). Its
///   bytes are unknown **forever** — which is not the same as *zero*, and a total is a question
///   about bytes. This kind used to be exempted, on the reasoning that "the total is as complete as
///   this file can ever be, so cache it". Two things are wrong with that. The number is not the
///   file's: the same deterministic failure also kills the file's COMBINED build, so what gets
///   cached is a per-import sum with no shared-module deduplication — every measured import's
///   contribution changes. And the exemption then let that number through *every* downstream gate
///   at once, since all of them read one flag: it was cached (L1), persisted to the no-TTL
///   bundle-impact history as the file's baseline, shown without the estimate label, and passed by
///   `importlens check` with **exit 0**. A floor is a floor whatever made it one.
///
/// Every one of them is named in the diagnostics either way: the user is owed the fact, and the
/// transient ones are owed the extra sentence that says a retry may fix them.
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
                crate::pipeline::stage::FILE_SIZE_FALLBACK,
                "import size is still being measured, so it is not counted in this file's \
                 conservative total"
                    .to_owned(),
                vec![specifier],
            ));
            continue;
        };

        let Some(sizes) = result.sizes() else {
            // No size, so no bytes, so the sum is short — whatever the stage. The stage decides
            // only what the user is told, never whether the total is a floor.
            totals.missing_inputs = true;
            let stage = result
                .unmeasured_stage()
                .unwrap_or(crate::pipeline::stage::FILE_SIZE_FALLBACK);
            let mut details = vec![specifier];
            details.push(if crate::engine::stage::is_transient(stage) {
                "this import's own build failed transiently, so its bytes are unknown for this run \
                 and the file's total is a floor"
                    .to_owned()
            } else {
                "this import could not be measured, so its bytes are missing from the file's total, \
                 which is a floor"
                    .to_owned()
            });
            diagnostics.push(diagnostic(
                stage,
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "import could not be measured".to_owned()),
                details,
            ));
            continue;
        };

        totals.sized_any = true;
        totals.raw_bytes += sizes.raw_bytes;
        totals.minified_bytes += sizes.minified_bytes;
        totals.gzip_bytes += sizes.gzip_bytes;
        totals.brotli_bytes += sizes.brotli_bytes;
        totals.zstd_bytes += sizes.zstd_bytes;
    }

    totals
}

/// The real conservative-fallback path — `per_import_totals` folded through `absorb_fallback` —
/// as one call, for the crate's tests.
///
/// The caching gate has to be tested against the total the code actually BUILDS. A hand-assembled
/// `FileSizeComputation` cannot fail when the fold is wrong, and the fold is where the defect
/// ADR-0006 §4 names lives: it is what decides whether an import that was never measured leaves a
/// mark on the total.
#[cfg(test)]
pub(crate) fn per_import_totals_for_test(sized: &[SizedImport]) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    let fallback = per_import_totals(sized, &mut diagnostics);
    let mut totals = FileSizeComputation::default();
    totals.absorb_fallback(fallback);
    totals.diagnostics = diagnostics;
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
    use crate::ipc::protocol::{ImportKind, MeasuredSizes};

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
        let mut result = ImportResult::measured(
            specifier,
            MeasuredSizes {
                raw_bytes: bytes,
                minified_bytes: bytes,
                gzip_bytes: bytes,
                brotli_bytes: bytes,
                zstd_bytes: bytes,
            },
        );
        result.truly_treeshakeable = true;
        result
    }

    fn measured(specifier: &str, bytes: u64) -> SizedImport {
        SizedImport {
            request: request(specifier),
            result: Some(result(specifier, bytes)),
        }
    }

    /// The shape a TIMEOUT/PANIC leaves behind now: Unmeasured. No size at all — not the entry
    /// file measured alone, not a zero.
    fn unmeasured(specifier: &str, stage: &str) -> SizedImport {
        SizedImport {
            request: request(specifier),
            result: Some(ImportResult::unmeasured(
                specifier,
                stage,
                "engine build did not complete within 8s",
                Vec::new(),
            )),
        }
    }

    fn absorb(sized: &[SizedImport]) -> FileSizeComputation {
        per_import_totals_for_test(sized)
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

    /// The defect ADR-0006 §4 names, and the one the hand-built `FileSizeComputation` in
    /// `service.rs` cannot see: with the static fallback deleted, a timed-out import arrives here
    /// as an ordinary Unmeasured result — `error: Some`, no size — and the old code's `error`
    /// branch `continue`d **past** the transient scan on the assumption that an error is always
    /// deterministic. The file's total would then silently drop that import's bytes and be cached
    /// as the file's size for the whole L1 TTL.
    #[test]
    fn a_transiently_unmeasured_import_makes_the_total_a_floor_and_is_never_cached() {
        for transient in [stage::TIMEOUT, stage::PANIC, stage::ENGINE_GONE] {
            let totals = absorb(&[measured("alpha", 100), unmeasured("beta", transient)]);

            assert_eq!(
                totals.raw_bytes, 100,
                "`{transient}`: the unmeasured import contributes NO bytes — there are none"
            );
            assert!(
                totals.incomplete,
                "`{transient}`: beta may well measure fine next time, so this total is a floor"
            );
            assert!(
                totals
                    .diagnostics
                    .iter()
                    .any(|item| stage::is_transient(&item.stage)),
                "`{transient}`: the import's transient stage must reach the aggregate: {:?}",
                totals.diagnostics
            );
            assert!(
                !totals.is_cacheable(),
                "`{transient}`: caching a floor serves it as the file's size for the whole TTL"
            );
        }
    }

    /// **The seventh instance.** An import that failed DETERMINISTICALLY was exempted here: it
    /// contributes zero, and the total was left `incomplete: false` on the reasoning that the
    /// number is "as complete as this file can ever be".
    ///
    /// It is not. Deterministically-unknown bytes are still unknown, and the SAME failure also
    /// killed the file's combined build — so the total on offer is an un-deduplicated per-import
    /// sum, a number the file never had. With the flag clear it was cached (L1), persisted to the
    /// no-TTL bundle-impact history as this file's baseline, shown with no estimate label, and
    /// passed by `importlens check` with exit 0. ADR-0006 invariant 4 admits no exception, and now
    /// neither does this.
    #[test]
    fn a_deterministically_unmeasured_import_makes_the_total_a_floor_and_is_never_cached() {
        for deterministic in [stage::PARSE, stage::LINK, stage::MISSING_EXPORT] {
            let totals = absorb(&[measured("alpha", 100), unmeasured("beta", deterministic)]);

            assert_eq!(
                totals.raw_bytes, 100,
                "`{deterministic}`: the unmeasured import contributes NO bytes"
            );
            assert!(
                totals.incomplete,
                "`{deterministic}`: beta's bytes are unknown, and unknown-forever is still unknown"
            );
            assert!(
                !totals.is_cacheable(),
                "`{deterministic}`: caching a floor serves it as the file's size for the whole TTL"
            );
            assert!(
                totals.diagnostics.iter().any(|item| item
                    .details
                    .iter()
                    .any(|detail| detail == "specifier: beta")),
                "the user is owed the specifier that is missing: {:?}",
                totals.diagnostics
            );
        }
    }

    /// The floor rule is about MEASUREMENT, not about failure: a file whose every import really was
    /// measured is complete, and must stay cacheable. Without this the fix above could be "made to
    /// pass" by flagging everything.
    #[test]
    fn a_file_whose_every_import_was_measured_is_not_a_floor() {
        let totals = absorb(&[
            measured("alpha", 100),
            measured("beta", 20),
            measured("gamma", 3),
        ]);

        assert_eq!(totals.raw_bytes, 123);
        assert!(!totals.incomplete);
        assert!(totals.is_cacheable());
    }
}
