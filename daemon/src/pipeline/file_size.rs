use crate::{
    cache::key::{
        FileFingerprint, fingerprints_are_reusable, sort_and_dedup_fingerprints,
        unverifiable_file_fingerprint,
    },
    engine::{BundleEntry, BundlePurpose, BundleRequest, boundary},
    ipc::protocol::{
        AssetContribution, ImportDiagnostic, ImportRequest, ImportResult, ImportRuntime,
        ModuleContribution,
    },
    pipeline::{
        analyze::{AnalysisContext, engine_selection},
        assets::{asset_diagnostics, process_assets_bounded},
        compress::{CompressionSizes, compress_all},
        minify::minify_source,
        resolver::resolve_package_entry,
        util::diagnostic,
    },
};
use std::collections::{BTreeMap, HashMap};

/// What the daemon knows about the package behind an import, before any build runs.
///
/// The two non-`Installed` kinds both mean "no `node_modules/<name>/package.json`" but are
/// different facts and must stay distinct. [`crate::pipeline::resolver::FirstPartySourceProbe`]
/// decides between them on positive evidence that the specifier is first-party, never on the
/// absence of a `package.json` declaration.
#[derive(Debug, Clone)]
pub enum SizedPackage {
    /// Installed: the daemon resolved its manifest and built a request (a request carries the
    /// installed version), so this import is an **entry** of the file's combined build.
    Installed(ImportRequest),
    /// **Not installed** and not first-party: the specifier resolves to nothing (an uninstalled
    /// dependency, a typo, a stale import). Its bytes belong in the total and are missing, so the
    /// total is a floor (SRS FR-024a, bullet 4), whether or not `package.json` declares it.
    NotInstalled,
    /// **Not a package**: a tsconfig path alias (`@app/components`, `~lib/foo`, or a bare
    /// `components/Button` under a `baseUrl`) resolving to first-party source. Import Lens measures
    /// third-party imports (ADR-0004), so it contributes nothing, like a relative import. It is
    /// not a gap and flags nothing; flagging it would make every aliased file a permanent floor.
    ///
    /// (`@/…`, `~/…`, `#…` and `$…` never get this far: `document::specifier` drops them before
    /// detection. Only alias forms that look like package names reach here.)
    PathAlias,
}

/// One import a file-size computation must account for, together with whatever the caller has
/// already measured for it.
///
/// `result` is `None` while that import's own build is in flight (the streaming handlers answer
/// from cache and let misses land later, `ipc::server`). Such an import is still an entry of the
/// combined build, but contributes nothing to the per-import fallback, which then is
/// [`FileSizeComputation::incomplete`] and never cached.
///
/// A [`SizedPackage::NotInstalled`] import is not an entry of any build and makes the total a
/// floor, like every other unmeasured contributor (SRS FR-024a, bullet 4).
///
/// The measurement is carried in, never re-derived, so the fallback never enters the engine.
#[derive(Debug, Clone)]
pub struct SizedImport {
    pub package: SizedPackage,
    /// The specifier as written in the source; every import has one, installed or not.
    pub specifier: String,
    pub result: Option<ImportResult>,
}

impl SizedImport {
    /// An import whose package IS installed, so the daemon could build a request for it.
    pub fn installed(request: ImportRequest, result: Option<ImportResult>) -> Self {
        Self {
            specifier: request.specifier.clone(),
            package: SizedPackage::Installed(request),
            result,
        }
    }

    /// An import whose package is **not installed** and whose specifier is not first-party source
    /// either. It contributes no bytes and cannot be measured, so it makes the file's total a floor
    /// (SRS FR-024a).
    pub fn not_installed(specifier: impl Into<String>) -> Self {
        Self {
            package: SizedPackage::NotInstalled,
            specifier: specifier.into(),
            result: None,
        }
    }

    /// An import whose specifier is a **path alias**, not a package. It flags nothing.
    pub fn path_alias(specifier: impl Into<String>) -> Self {
        Self {
            package: SizedPackage::PathAlias,
            specifier: specifier.into(),
            result: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct FileSizeComputation {
    pub raw_bytes: u64,
    pub minified_bytes: u64,
    pub gzip_bytes: u64,
    pub brotli_bytes: u64,
    pub zstd_bytes: u64,
    /// What the totals above are made of, per non-JavaScript kind; already inside the five sizes.
    ///
    /// One row per kind, summed across runtime groups: each group's assets are compressed on their
    /// own (ADR-0005).
    pub asset_breakdown: Vec<AssetContribution>,
    /// Bytes that belong in these totals are absent: an import contributed no measurement, or a
    /// successful build disclosed a floor ([`crate::pipeline::stage::marks_a_floor`]). The totals are then a lower bound:
    /// safe to show beside the diagnostics that say so (FR-024a: a floor beats a zero), never safe
    /// to persist or compare against a baseline (ADR-0006, invariant 4). Only a deterministic floor
    /// may sit in the L1 aggregate cache, flagged ([`Self::may_enter_aggregate_cache`]).
    ///
    /// **Any** missing-byte shape sets it:
    ///
    /// * **Loading**: its own build had not landed when the sum was taken (`result: None`).
    /// * **Unmeasured, transient**: timeout / panic / engine_gone.
    /// * **Unmeasured, deterministic**: parse / link / missing_export / …. This says nothing about
    ///   how many bytes the import contributes, which is the only question a total asks.
    /// * **Unresolved**: not an entry of the combined build, so its bytes are absent however well
    ///   that build went.
    /// * **Measured but partial**: `uncounted_assets` (supported shipped assets absent from the
    ///   sizes), `resolve` (an unresolvable specifier kept as an import boundary) or
    ///   `missing_export` (a stubbed binding) on a build that succeeded.
    ///
    /// No other signal sees this: `error` is `None` (the sum succeeded), and the stage scan in
    /// [`Self::is_file_cost`] sees only request-local stages (a still-building import has no stage,
    /// and a floor disclosure is deliberately reusable at the import level).
    pub incomplete: bool,
    /// The file's own combined build failed, so these totals fell back to a sum of per-import costs
    /// with no shared-module deduplication. That is a different quantity from a File Cost
    /// ([ADR-0004]): a shared module is counted twice. An over-count, not a floor, and just as
    /// unusable: never cached, persisted, or judged (ADR-0006, invariant 4, second half).
    ///
    /// `incomplete` cannot see this. The combined build is the likeliest build to hit
    /// `BUILD_TIMEOUT`, and when it does every import may still be Measured and cached, so
    /// `missing_inputs` is `false` and `error` is `None`.
    ///
    /// Set for a deterministic combined-build failure too: a false fail is also a verdict, and
    /// invariant 5 forbids judging a budget against a number the file never had.
    pub degraded: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Exact inputs of the combined build, retained only for the process-local File Cost cache.
    /// This is not part of the wire or disk schema.
    pub(crate) dependency_fingerprints: Vec<FileFingerprint>,
}

impl FileSizeComputation {
    /// Whether this aggregate is a measurement of the file: the rule every durable record and
    /// verdict applies (SRS FR-026c), stated again by the extension and the CLI.
    ///
    /// It is not when it failed (`error`), is [`Self::incomplete`] (an under-count), is
    /// [`Self::degraded`] (an un-deduplicated over-count), carries unverifiable fingerprints, or
    /// carries a transient stage on a diagnostic.
    ///
    /// `degraded` is redundant with neither `incomplete` (a combined build can park while every
    /// import is measured) nor the transient scan (a deterministic combined-build failure carries
    /// a durable stage such as `link` or `parse`).
    pub fn is_file_cost(&self) -> bool {
        self.error.is_none()
            && !self.incomplete
            && !self.degraded
            && fingerprints_are_reusable(&self.dependency_fingerprints)
            && !self
                .diagnostics
                .iter()
                .any(|item| crate::pipeline::stage::is_transient(&item.stage))
    }

    /// Whether this aggregate may be written to the L1 file-size cache (SRS FR-026c), which
    /// [`crate::pipeline::file_size_cache::FileSizeCache::insert`] asks itself: a File Cost, or a
    /// floor whose missing bytes are deterministic (an import not installed, unresolvable, or
    /// failing on its own bytes). Such a floor is the same floor on every read until an input
    /// changes, and the cache's signature, fingerprints and 30-second window expire it; it keeps
    /// its `incomplete` flag, so no record or verdict ever takes it for a measurement.
    ///
    /// A floor qualifies only when every stage it carries is on the durable allowlist (FR-026c).
    /// That refuses a degraded sum, a total missing a still-Loading import (`file_size_fallback` is
    /// not on the list), and any transient or unclassified stage: those change on the next read by
    /// themselves.
    pub fn may_enter_aggregate_cache(&self) -> bool {
        self.is_file_cost()
            || (self.error.is_none()
                && !self.degraded
                && fingerprints_are_reusable(&self.dependency_fingerprints)
                && self
                    .diagnostics
                    .iter()
                    .all(|item| crate::pipeline::stage::may_enter_a_durable_store(&item.stage)))
    }

    /// Fold one runtime group's conservative per-import sum into the file's totals, and report
    /// whether it contributed anything.
    ///
    /// The only way a fallback sum reaches the totals: the missing-input flag travels with the
    /// bytes and is applied here, so a caller cannot add the bytes and forget the flag.
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
        self.absorb_asset_breakdown(&fallback.asset_breakdown);
        true
    }

    /// Merge one source of per-kind asset weight into the file's composition, summing by kind.
    ///
    /// Fed by both a runtime group's combined build and a degraded group's per-import fallback,
    /// which still has real asset bytes in its total.
    fn absorb_asset_breakdown(&mut self, contributions: &[AssetContribution]) {
        absorb_asset_breakdown_into(&mut self.asset_breakdown, contributions);
    }
}

/// Sum per-kind asset weight into a breakdown, matching on kind.
///
/// One definition for both accumulators (the file's composition and the per-import fallback's), so
/// a new size field cannot be added to one and silently dropped from the other.
fn absorb_asset_breakdown_into(
    breakdown: &mut Vec<AssetContribution>,
    contributions: &[AssetContribution],
) {
    for contribution in contributions {
        match breakdown
            .iter_mut()
            .find(|existing| existing.kind == contribution.kind)
        {
            Some(existing) => {
                existing.raw_bytes += contribution.raw_bytes;
                existing.minified_bytes += contribution.minified_bytes;
                existing.gzip_bytes += contribution.gzip_bytes;
                existing.brotli_bytes += contribution.brotli_bytes;
                existing.zstd_bytes += contribution.zstd_bytes;
            }
            None => breakdown.push(*contribution),
        }
    }
}

/// How much of each import's weight another import of the same document also pulls in, counted
/// **within a runtime** only.
///
/// A module reached from Astro frontmatter (Server) and from a client `<script>` (Client) is not
/// shared: the two runtimes ship as separate artifacts, each with its own copy ([ADR-0005]), and
/// `insights.ts` would render a cross-runtime count as a saving the build never makes. The runtime
/// arrives with each result, never re-derived, so the partition has one source.
pub fn annotate_shared_bytes<'a>(
    imports: impl IntoIterator<Item = (ImportRuntime, &'a mut ImportResult)>,
) {
    let mut imports = imports.into_iter().collect::<Vec<_>>();
    let mut counts = HashMap::<ImportRuntime, HashMap<String, usize>>::new();

    for (runtime, result) in &imports {
        let within = counts.entry(*runtime).or_default();
        for module in result_contributions(result) {
            *within.entry(module.path.clone()).or_default() += 1;
        }
    }

    for (runtime, result) in &mut imports {
        let within = counts.get(runtime);
        let shared = result_contributions(result)
            .iter()
            .filter(|module| {
                within
                    .and_then(|within| within.get(&module.path))
                    .copied()
                    .unwrap_or_default()
                    > 1
            })
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
/// A `BundleRequest` carries a single runtime, and Rolldown resolves the whole transitive graph
/// under it. Server and Client resolve dependencies under different conditions (`browser` alias
/// fields, `browser` vs `node` export conditions), and a mis-conditioned build still succeeds
/// silently, so every entry must be built under its own runtime. A single Astro file mixes them:
/// frontmatter imports are Server, processed `<script>` imports are Client.
///
/// Grouping is per runtime, not per entry: shared-module deduplication is real only within a
/// runtime.
///
/// Each group is minified and **compressed on its own, and the results are added**: compressed
/// bytes may be summed across an artifact boundary, never within one ([ADR-0005], [ADR-0004]).
/// Only Astro documents mix runtimes; every other document has one `Component` group.
pub fn compute_file_size(
    context: &AnalysisContext,
    imports: &[SizedImport],
) -> FileSizeComputation {
    compute_file_size_with(context, imports, &minify_source, &|code| {
        compress_all(code).map_err(|error| error.to_string())
    })
}

/// [`compute_file_size`] with the minifier injected, which no production caller does.
///
/// The minify-failure arm below degrades the totals exactly as a build failure does, and no fixture
/// can reach it: Rolldown parses every module with the same OXC parser [`minify_source`] uses, in
/// strict module mode, so a source that would fail the re-parse fails the build first (legacy
/// octal, `with`, duplicate parameters, `delete` of a local, and a hashbang all fail as `parse`).
/// The arm still guards a codegen/minifier defect, so this seam exists for its one test.
fn compute_file_size_with(
    context: &AnalysisContext,
    imports: &[SizedImport],
    minify: &dyn Fn(&str) -> Result<String, String>,
    compress: &dyn Fn(&str) -> Result<CompressionSizes, String>,
) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    let mut totals = FileSizeComputation::default();
    // Entries and their imports, grouped by runtime. `BTreeMap` keeps the output deterministic.
    let mut groups: BTreeMap<ImportRuntime, RuntimeGroup> = BTreeMap::new();

    for import in imports {
        let specifier = format!("specifier: {}", import.specifier);
        let request = match &import.package {
            SizedPackage::Installed(request) => request,
            SizedPackage::NotInstalled => {
                // Not installed and not first-party: its bytes are missing however cleanly every
                // build goes. Floor (SRS FR-024a, bullet 4).
                totals.incomplete = true;
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::PACKAGE_RESOLUTION,
                    "package is not installed, so its bytes are missing from this file's total, \
                     which is a floor"
                        .to_owned(),
                    vec![specifier],
                ));
                continue;
            }
            SizedPackage::PathAlias => {
                // A path alias to first-party source contributes nothing (ADR-0004), like a
                // relative import. A fact, not a gap: no flag, and the total stays complete.
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::PATH_ALIAS,
                    "specifier is a path alias resolving to first-party source, not an installed \
                     package; Import Lens measures third-party imports, so it contributes no bytes \
                     to this file's total"
                        .to_owned(),
                    vec![specifier],
                ));
                continue;
            }
        };

        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => {
                let group = groups.entry(request.runtime).or_default();
                group.entries.push(BundleEntry {
                    entry_path: resolved.entry_path.clone(),
                    package_root: resolved.package_root.clone(),
                    selection: engine_selection(request),
                });
                group.sized.push(import.clone());
            }
            // A declarations-only package resolves to `Err` by design (it ships no runtime code),
            // and `pipeline::types_only` answers it Measured at zero. Contributing no bytes is a
            // fact, so the total stays complete; treating it as a gap would make every file that
            // imports `@types/…` a permanent floor.
            Err(_)
                if import
                    .result
                    .as_ref()
                    .is_some_and(ImportResult::is_types_only) =>
            {
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::TYPES_ONLY,
                    "package contains declarations only; it contributes zero runtime bytes to this \
                     file, which is a measurement and not a gap"
                        .to_owned(),
                    vec![specifier],
                ));
            }
            // A native-binary-only package likewise ships no importable JS entry, and
            // `pipeline::native_binary` answers it Measured at zero. The total stays complete.
            Err(_)
                if import
                    .result
                    .as_ref()
                    .is_some_and(ImportResult::is_native_binary_only) =>
            {
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::NATIVE_BINARY_ONLY,
                    "package ships only a native binary; it contributes zero runtime bytes to this \
                     file, which is a measurement and not a gap"
                        .to_owned(),
                    vec![specifier],
                ));
            }
            Err(error) => {
                // Not an entry of any group, so its bytes are missing however cleanly the
                // combined builds go. Floor (ADR-0006, invariant 4).
                totals.incomplete = true;
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::ENTRY_RESOLUTION,
                    error,
                    vec![specifier],
                ));
            }
        }
    }

    if groups.is_empty() {
        // No combined build to run: no imports, or only declarations-only/native-binary-only/path
        // alias imports (a complete zero), or none resolved (`incomplete`, never cached).
        return FileSizeComputation {
            diagnostics,
            ..totals
        };
    }

    // Each runtime group is minified and compressed on its own, and the results are added. A
    // runtime is an artifact boundary (ADR-0005): Server and Client bundles ship and compress
    // separately. Never concatenate groups before compressing: that compresses away redundancy
    // between payloads that never meet, under-reporting what ships.
    let mut any_sized = false;

    for (runtime, group) in groups {
        let artifact = match boundary::bundle_sync(BundleRequest {
            entries: group.entries,
            runtime,
            purpose: BundlePurpose::FileSize,
        }) {
            Ok(artifact) => artifact,
            Err(failure) => {
                // Only this runtime degrades; other groups keep their deduplicated numbers. The
                // file's totals are then part bundle, part per-import sum, so `degraded` is set
                // for any failure stage, deterministic ones included.
                totals.degraded = true;
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

        // Never call `record_loaded_paths` here: this build's `loaded_paths` is the union over the
        // group, and writing it under each entry's key would clobber the per-entry sets
        // `analyze.rs` records, so an edit to one package would invalidate unrelated sizes.
        diagnostics.extend(artifact.diagnostics.iter().map(|item| ImportDiagnostic {
            stage: item.stage.clone(),
            message: item.message.clone(),
            details: Vec::new(),
        }));

        let minified = match minify(&artifact.code) {
            Ok(minified) => minified,
            Err(error) => {
                // Degrade only this runtime, as a build failure does; returning would discard
                // every other group's real totals.
                totals.degraded = true;
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

        let compressed = match compress(&minified) {
            Ok(compressed) => compressed,
            Err(error) => {
                // Degrade only this runtime, as the build and minify arms do: one group's
                // compressor failing says nothing about another group's bytes.
                totals.degraded = true;
                diagnostics.push(diagnostic(
                    crate::pipeline::stage::COMPRESSION,
                    error,
                    vec![
                        "compression failed for this runtime; its totals are conservative \
                         per-import sums without shared-module deduplication"
                            .to_owned(),
                    ],
                ));
                let fallback = per_import_totals(&group.sized, &mut diagnostics);
                any_sized |= totals.absorb_fallback(fallback);
                continue;
            }
        };

        // This group's non-JavaScript assets, processed the way they ship. The combined build
        // saw every import in this runtime, so its stylesheets bundle into one artifact, deduping
        // what two imports both `@import`. Each artifact is compressed on its own and summed
        // (ADR-0005); an asset that cannot be processed is disclosed.
        let assets = match process_assets_bounded(
            artifact.assets.clone(),
            artifact.graph_source_bytes,
            artifact.loaded_paths.clone(),
        ) {
            Ok(assets) => assets,
            Err(failure) => {
                // The asset tail produced no coherent measurement: degrade this group like a
                // combined-build failure.
                totals.degraded = true;
                diagnostics.push(diagnostic(
                    failure.stage,
                    failure.message,
                    vec![
                        "asset processing failed for this runtime; its totals are conservative \
                         per-import sums without shared-module or shared-asset deduplication"
                            .to_owned(),
                    ],
                ));
                let fallback = per_import_totals(&group.sized, &mut diagnostics);
                any_sized |= totals.absorb_fallback(fallback);
                continue;
            }
        };
        let asset_sizes = assets.total();
        totals.absorb_asset_breakdown(&assets.contributions);
        totals
            .dependency_fingerprints
            .extend(artifact.read_time_fingerprints.iter().cloned());
        totals
            .dependency_fingerprints
            .extend(assets.freshness_fingerprints());
        // First-party manifests are freshness inputs, as on the per-import path: `sideEffects` and
        // `exports` change this number without moving a module byte, and the extension's watcher
        // globs only `**/node_modules/*/package.json`.
        totals.dependency_fingerprints.extend(
            crate::pipeline::analyze::first_party_manifests(context, &artifact.loaded_paths)
                .into_iter()
                .filter_map(crate::cache::key::file_fingerprint_reading_hash),
        );
        totals.dependency_fingerprints.extend(
            artifact
                .unhashed_paths
                .iter()
                .map(unverifiable_file_fingerprint),
        );
        for disclosure in asset_diagnostics(&assets) {
            diagnostics.push(diagnostic(
                &disclosure.stage,
                disclosure.message,
                disclosure.details,
            ));
        }

        // A floor disclosure from the build or its asset stage, or any engine-emitted asset, means
        // shipped bytes are absent from all five totals: a floor, never cached, persisted, or
        // judged as File Cost (though each deterministic disclosure stays reusable per import).
        totals.incomplete |= !artifact.emitted_assets.is_empty()
            || assets.has_uncounted_assets()
            || artifact
                .diagnostics
                .iter()
                .any(|diagnostic| crate::pipeline::stage::marks_a_floor(&diagnostic.stage));

        any_sized = true;
        totals.raw_bytes += artifact.code.len() as u64 + asset_sizes.raw_bytes;
        // `minified_bytes` is measured on the same string this group's compressors saw.
        totals.minified_bytes += minified.len() as u64 + asset_sizes.minified_bytes;
        totals.gzip_bytes += compressed.gzip_bytes + asset_sizes.gzip_bytes;
        totals.brotli_bytes += compressed.brotli_bytes + asset_sizes.brotli_bytes;
        totals.zstd_bytes += compressed.zstd_bytes + asset_sizes.zstd_bytes;
    }

    if !any_sized {
        return error_computation(
            &totals,
            crate::pipeline::stage::FILE_SIZE_FALLBACK,
            "no import could be sized conservatively".to_owned(),
            diagnostics,
        );
    }

    sort_and_dedup_fingerprints(&mut totals.dependency_fingerprints);

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
    /// Bytes that belong in this sum are absent: an import was not Measured, or was Measured as a
    /// floor. The sum then falls short of the file by an unknown amount.
    missing_inputs: bool,
    raw_bytes: u64,
    minified_bytes: u64,
    gzip_bytes: u64,
    brotli_bytes: u64,
    zstd_bytes: u64,
    /// The per-kind composition of the imports in this sum, so a degraded runtime group still tells
    /// the user what its number is made of.
    asset_breakdown: Vec<AssetContribution>,
}

/// When a package breaks the combined build, a runtime group degrades to conservative
/// non-deduplicated per-import totals instead of zeroing the aggregate (SRS FR-024a).
///
/// It sums the measurements the caller already has and **never enters the engine**, so nothing
/// here can park.
///
/// Only a Measured import contributes bytes (ADR-0006: a size exists if and only if a build
/// succeeded). Every other kind makes the sum a floor (`missing_inputs`, invariant 4, with no
/// exception):
///
/// * **Loading** (`result: None`): short by that import's weight.
/// * **Unmeasured, transient** (`timeout` / `panic` / `engine_gone`): unknown for this run only.
/// * **Unmeasured, deterministic** (`parse`, `link`, `missing_export`, `oversized_entry`, …):
///   unknown forever, which is not zero. The same failure also kills the combined build, so the
///   sum is not the file's number either.
///
/// Each is named in the diagnostics; transient ones also say a retry may fix them.
fn per_import_totals(
    sized: &[SizedImport],
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> PerImportTotals {
    let mut totals = PerImportTotals::default();

    for import in sized {
        let specifier = format!("specifier: {}", import.specifier);
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
            // No size means the sum is short, whatever the stage. The stage decides only what
            // the user is told.
            totals.missing_inputs = true;
            let stage = result
                .unmeasured_stage()
                .unwrap_or(crate::pipeline::stage::FILE_SIZE_FALLBACK);
            let mut details = vec![specifier];
            details.push(if crate::pipeline::stage::is_transient(stage) {
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

        if result.is_floor() {
            totals.missing_inputs = true;
            for disclosure in result
                .diagnostics
                .iter()
                .filter(|diagnostic| crate::pipeline::stage::marks_a_floor(&diagnostic.stage))
            {
                let mut details = vec![specifier.clone()];
                details.extend(disclosure.details.iter().cloned());
                diagnostics.push(diagnostic(
                    &disclosure.stage,
                    disclosure.message.clone(),
                    details,
                ));
            }
        }

        totals.sized_any = true;
        totals.raw_bytes += sizes.raw_bytes;
        totals.minified_bytes += sizes.minified_bytes;
        totals.gzip_bytes += sizes.gzip_bytes;
        totals.brotli_bytes += sizes.brotli_bytes;
        totals.zstd_bytes += sizes.zstd_bytes;
        // These asset bytes are already inside `sizes`; the rows only describe composition. Like
        // the sum, they do not deduplicate a stylesheet two imports share.
        absorb_asset_breakdown_into(&mut totals.asset_breakdown, &result.asset_breakdown);
    }

    totals
}

/// The real conservative-fallback path (`per_import_totals` folded through `absorb_fallback`) as
/// one call, for the crate's tests, so the caching gate is tested against the total the code
/// builds.
///
/// It never runs a combined build, so `degraded` is always `false` here: tests of the degraded
/// shape must go through [`compute_file_size`].
#[cfg(test)]
pub(crate) fn per_import_totals_for_test(sized: &[SizedImport]) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    let fallback = per_import_totals(sized, &mut diagnostics);
    let mut totals = FileSizeComputation::default();
    totals.absorb_fallback(fallback);
    totals.diagnostics = diagnostics;
    totals
}

/// The aggregate failed outright: no bytes at all.
///
/// It carries `incomplete` and `degraded` forward rather than resetting them, so the wire never
/// claims `incomplete: false` about a total already known to be missing an import.
fn error_computation(
    totals: &FileSizeComputation,
    stage: &str,
    message: String,
    mut diagnostics: Vec<ImportDiagnostic>,
) -> FileSizeComputation {
    diagnostics.push(diagnostic(stage, message.clone(), Vec::new()));

    FileSizeComputation {
        error: Some(message),
        diagnostics,
        incomplete: totals.incomplete,
        degraded: totals.degraded,
        ..FileSizeComputation::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::stage;
    use crate::ipc::protocol::{ImportKind, MeasuredSizes};
    use std::path::PathBuf;

    /// Two imports of the same UI kit pull ONE stylesheet, and the combined File Cost bundles it
    /// once, so Combined Import Cost exceeds File Cost by that sheet. `shared_bytes` must see it
    /// even though a stylesheet links as an empty module.
    #[test]
    fn a_stylesheet_two_imports_share_is_counted_as_shared_weight() {
        let sheet = "/pkg/ui-kit/styles.css".to_owned();
        let mut first = ImportResult::measured("ui-kit", MeasuredSizes::ZERO);
        let mut second = ImportResult::measured("ui-kit", MeasuredSizes::ZERO);
        for result in [&mut first, &mut second] {
            result.internal_contributions = vec![ModuleContribution {
                path: sheet.clone(),
                bytes: 36_000,
            }];
        }

        annotate_shared_bytes([
            (ImportRuntime::Client, &mut first),
            (ImportRuntime::Client, &mut second),
        ]);

        assert_eq!(
            first.shared_bytes,
            Some(36_000),
            "a stylesheet both imports pull is shared weight, not weight each carries alone"
        );
        assert_eq!(second.shared_bytes, Some(36_000));
    }

    /// The counterpart: a runtime is an artifact boundary (ADR-0005), so a Server import and a
    /// Client import each ship their own copy and share nothing. Without this the test above would
    /// pass just as well on an implementation that ignored the runtime entirely.
    #[test]
    fn a_stylesheet_pulled_from_two_runtimes_is_not_shared() {
        let sheet = "/pkg/ui-kit/styles.css".to_owned();
        let mut client = ImportResult::measured("ui-kit", MeasuredSizes::ZERO);
        let mut server = ImportResult::measured("ui-kit", MeasuredSizes::ZERO);
        for result in [&mut client, &mut server] {
            result.internal_contributions = vec![ModuleContribution {
                path: sheet.clone(),
                bytes: 36_000,
            }];
        }

        annotate_shared_bytes([
            (ImportRuntime::Client, &mut client),
            (ImportRuntime::Server, &mut server),
        ]);

        assert_eq!(
            client.shared_bytes,
            Some(0),
            "runtimes ship separate copies"
        );
        assert_eq!(server.shared_bytes, Some(0));
    }

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
        SizedImport::installed(request(specifier), Some(result(specifier, bytes)))
    }

    /// The shape a timeout or panic leaves behind: Unmeasured, with no size at all.
    fn unmeasured(specifier: &str, stage: &str) -> SizedImport {
        SizedImport::installed(
            request(specifier),
            Some(ImportResult::unmeasured(
                specifier,
                stage,
                "engine build did not complete within 8s",
                Vec::new(),
            )),
        )
    }

    fn absorb(sized: &[SizedImport]) -> FileSizeComputation {
        per_import_totals_for_test(sized)
    }

    /// The production compressor, for the tests that inject only the *other* hook.
    fn real_compress(code: &str) -> Result<CompressionSizes, String> {
        compress_all(code).map_err(|error| error.to_string())
    }

    fn runtime_request(specifier: &str, runtime: ImportRuntime) -> ImportRequest {
        ImportRequest {
            runtime,
            ..request(specifier)
        }
    }

    #[test]
    fn a_sum_of_real_measurements_is_the_file_and_is_file_cost() {
        let totals = absorb(&[measured("alpha", 100), measured("beta", 20)]);

        assert_eq!(totals.raw_bytes, 120);
        assert!(!totals.incomplete);
        assert!(
            totals.is_file_cost(),
            "every import was really measured, so the sum IS this file's size"
        );
    }

    #[test]
    fn conflicting_dependency_snapshots_are_not_cacheable() {
        let mut totals = FileSizeComputation::default();
        let first = FileFingerprint {
            path: "/pkg/font.woff2".to_owned(),
            len: 4,
            modified_millis: 10,
            content_hash: Some(crate::cache::key::content_hash(b"aaaa")),
        };
        totals.dependency_fingerprints = vec![
            first.clone(),
            FileFingerprint {
                content_hash: Some(crate::cache::key::content_hash(b"bbbb")),
                ..first
            },
        ];

        assert!(
            !totals.is_file_cost(),
            "no on-disk file can validate two different known snapshots of one path"
        );
    }

    /// A cold import's `result` is `None` when the combined build fails and the fallback sum is
    /// taken; a total short by one whole import must never be cached as the file's size.
    #[test]
    fn a_sum_missing_a_still_building_import_is_not_the_file_and_is_never_cached() {
        let totals = absorb(&[
            measured("alpha", 100),
            SizedImport::installed(request("beta"), None),
        ]);

        assert_eq!(totals.raw_bytes, 100, "the missing import contributes zero");
        assert!(totals.incomplete);
        assert!(
            !totals.is_file_cost(),
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

    /// ADR-0006 §4: a timed-out import arrives as an ordinary Unmeasured result (`error: Some`, no
    /// size); the total must not silently drop its bytes and be cached for the L1 TTL.
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
                    .any(|item| crate::pipeline::stage::is_transient(&item.stage)),
                "`{transient}`: the import's transient stage must reach the aggregate: {:?}",
                totals.diagnostics
            );
            assert!(
                !totals.is_file_cost(),
                "`{transient}`: caching a floor serves it as the file's size for the whole TTL"
            );
        }
    }

    /// A deterministically failed import's bytes are still unknown, and the same failure kills the
    /// combined build, so the sum is a number the file never had. ADR-0006 invariant 4 admits no
    /// exception.
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
                !totals.is_file_cost(),
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

    #[test]
    fn a_measured_import_with_uncounted_assets_makes_the_fallback_a_floor() {
        let mut partial = result("asset-lib", 100);
        partial.diagnostics.push(ImportDiagnostic {
            stage: crate::engine::diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
            message: "a stylesheet could not be processed and is absent from this size".to_owned(),
            details: vec!["broken.scss".to_owned()],
        });

        let totals = absorb(&[SizedImport::installed(request("asset-lib"), Some(partial))]);

        assert_eq!(
            totals.raw_bytes, 100,
            "the measured portion remains useful as a lower bound"
        );
        assert!(
            totals.incomplete,
            "a successful JavaScript build does not make omitted asset bytes part of the sum"
        );
        assert!(
            !totals.is_file_cost(),
            "a deterministic floor may be cached per import, but not as this file's complete cost"
        );
        assert!(
            totals.diagnostics.iter().any(|diagnostic| {
                diagnostic.stage == crate::engine::diagnostic_stage::UNCOUNTED_ASSETS
                    && diagnostic
                        .details
                        .iter()
                        .any(|detail| detail == "specifier: asset-lib")
            }),
            "the aggregate must retain which import left asset bytes out: {:?}",
            totals.diagnostics
        );
    }

    /// An unresolvable specifier kept as a boundary and a stubbed binding both leave the import
    /// Measured with bytes missing; a builtin boundary leaves nothing missing.
    #[test]
    fn a_measured_import_that_is_a_floor_makes_the_fallback_a_floor() {
        for (stage, is_floor) in [
            (crate::engine::stage::RESOLVE, true),
            (crate::engine::stage::MISSING_EXPORT, true),
            (crate::engine::diagnostic_stage::EXTERNAL, false),
        ] {
            let mut measured = result("boundary-lib", 100);
            measured.diagnostics.push(ImportDiagnostic {
                stage: stage.to_owned(),
                message: "disclosed beside the number".to_owned(),
                details: Vec::new(),
            });

            let totals = absorb(&[SizedImport::installed(
                request("boundary-lib"),
                Some(measured),
            )]);

            assert_eq!(totals.raw_bytes, 100, "{stage}");
            assert_eq!(totals.incomplete, is_floor, "{stage}");
            assert_eq!(totals.is_file_cost(), !is_floor, "{stage}");
        }
    }

    #[test]
    fn a_measured_import_with_imprecise_assets_is_not_a_floor() {
        let mut high = result("asset-lib", 100);
        high.diagnostics.push(ImportDiagnostic {
            stage: crate::engine::diagnostic_stage::IMPRECISE_ASSETS.to_owned(),
            message: "stylesheets were measured separately, so this size may read high".to_owned(),
            details: Vec::new(),
        });

        let totals = absorb(&[SizedImport::installed(request("asset-lib"), Some(high))]);

        assert!(
            !totals.incomplete,
            "an over-count is imprecise, but it is not missing asset bytes"
        );
    }

    /// The floor rule is about measurement, not failure: a file whose every import was measured is
    /// complete and cacheable, so flagging everything cannot satisfy the tests above.
    #[test]
    fn a_file_whose_every_import_was_measured_is_not_a_floor() {
        let totals = absorb(&[
            measured("alpha", 100),
            measured("beta", 20),
            measured("gamma", 3),
        ]);

        assert_eq!(totals.raw_bytes, 123);
        assert!(!totals.incomplete);
        assert!(totals.is_file_cost());
    }

    // ---------------------------------------------------------------------------------------
    // Through `compute_file_size` itself.
    //
    // `per_import_totals_for_test` never runs a combined build, so `degraded` cannot be expressed
    // through it. These use the real entry point, a real fixture, and a real Rolldown build.
    // ---------------------------------------------------------------------------------------

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "il-fs-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(root.join("src")).expect("workspace");
            std::fs::write(root.join("src").join("index.ts"), "// document\n").expect("document");
            Self { root }
        }

        /// An installed package whose entry is `source`. Invalid JavaScript here fails the combined
        /// Rolldown build deterministically at `parse`.
        fn package(&self, name: &str, source: &str) -> &Self {
            let package_root = self.root.join("node_modules").join(name);
            std::fs::create_dir_all(&package_root).expect("package dir");
            std::fs::write(
                package_root.join("package.json"),
                r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
            )
            .expect("manifest");
            std::fs::write(package_root.join("index.js"), source).expect("entry");
            self
        }

        fn package_file(
            &self,
            package: &str,
            relative_path: &str,
            contents: impl AsRef<[u8]>,
        ) -> &Self {
            let path = self
                .root
                .join("node_modules")
                .join(package)
                .join(relative_path);
            std::fs::create_dir_all(path.parent().expect("package file parent"))
                .expect("package file directory");
            std::fs::write(path, contents).expect("package file");
            self
        }

        /// A declarations-only package: a manifest, a `.d.ts`, and NO runtime entry. It resolves to
        /// `Err` by design.
        fn types_only_package(&self, name: &str) -> &Self {
            let package_root = self.root.join("node_modules").join(name);
            std::fs::create_dir_all(&package_root).expect("package dir");
            std::fs::write(
                package_root.join("package.json"),
                r#"{"version":"1.0.0","types":"index.d.ts"}"#,
            )
            .expect("manifest");
            std::fs::write(
                package_root.join("index.d.ts"),
                "export declare const a: number;\n",
            )
            .expect("declarations");
            self
        }

        /// A native-binary-only package: a manifest with a `bin` and a platform-specific native
        /// binary in `optionalDependencies`, and NO runtime entry. It resolves to `Err` by design.
        fn native_binary_only_package(&self, name: &str) -> &Self {
            let package_root = self.root.join("node_modules").join(name);
            std::fs::create_dir_all(&package_root).expect("package dir");
            std::fs::write(
                package_root.join("package.json"),
                r#"{"version":"1.0.0","bin":{"x":"bin/x"},"optionalDependencies":{"@scope/x-win32-x64":"1.0.0"}}"#,
            )
            .expect("manifest");
            self
        }

        fn context(&self) -> AnalysisContext {
            AnalysisContext {
                workspace_root: self.root.clone(),
                active_document_path: self.root.join("src").join("index.ts"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// The MEASURED zero a declarations-only package is answered with (`pipeline::types_only`).
    fn types_only_result(specifier: &str) -> ImportResult {
        let mut result = ImportResult::measured(specifier, MeasuredSizes::ZERO);
        result.diagnostics = vec![ImportDiagnostic {
            stage: crate::pipeline::stage::TYPES_ONLY.to_owned(),
            message: "package contains declarations only; zero runtime cost".to_owned(),
            details: Vec::new(),
        }];
        result
    }

    /// The MEASURED zero a native-binary-only package is answered with
    /// (`pipeline::native_binary`).
    fn native_binary_only_result(specifier: &str) -> ImportResult {
        let mut result = ImportResult::measured(specifier, MeasuredSizes::ZERO);
        result.diagnostics = vec![ImportDiagnostic {
            stage: crate::pipeline::stage::NATIVE_BINARY_ONLY.to_owned(),
            message: "package ships only a native binary; zero runtime cost".to_owned(),
            details: Vec::new(),
        }];
        result
    }

    /// A declarations-only package resolves to nothing because it ships nothing, and is answered
    /// Measured. Flagging its `Err` as a gap would make every `@types/…` importer a floor.
    #[test]
    fn a_types_only_import_is_a_measurement_and_leaves_its_file_complete() {
        let fixture = Fixture::new("types-only");
        fixture
            .package("real-lib", "export const value = 41 + 1;\n")
            .types_only_package("types-lib");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                SizedImport::installed(
                    request("real-lib"),
                    Some(result("real-lib", 10)), // its own number; the combined build makes the total
                ),
                SizedImport::installed(request("types-lib"), Some(types_only_result("types-lib"))),
            ],
        );

        assert!(
            totals.error.is_none(),
            "the real package builds; nothing failed: {:?}",
            totals.diagnostics
        );
        assert!(
            !totals.incomplete,
            "a types-only import contributes a genuine ZERO, not an unknown: {:?}",
            totals.diagnostics
        );
        assert!(!totals.degraded, "the combined build succeeded");
        assert!(
            totals.is_file_cost(),
            "a file whose only unresolvable import is types-only is fully measured, and must be \
             cached — otherwise every `@types`-importing file rebuilds on every size request"
        );
        assert!(totals.raw_bytes > 0, "the real package's bytes are counted");
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|item| item.stage == crate::pipeline::stage::TYPES_ONLY),
            "the user is still told why that import contributes nothing: {:?}",
            totals.diagnostics
        );
    }

    /// The combined build measures a graph whose dependency asks for a subpath its host refuses to
    /// export: the edge is kept as a boundary, so the file's total is a floor.
    #[test]
    fn a_combined_build_with_an_unresolvable_boundary_is_a_floor() {
        let fixture = Fixture::new("unresolved-boundary");
        fixture
            .package_file(
                "host-lib",
                "package.json",
                r#"{"version":"1.0.0","main":"index.js","exports":{".":"./index.js"}}"#,
            )
            .package_file("host-lib", "index.js", "export const open = 1;\n")
            .package_file(
                "host-lib",
                "internal/secret.js",
                "export const secret = 2;\n",
            )
            .package(
                "consumer-lib",
                "import { secret } from 'host-lib/internal/secret';\nexport const value = secret;\n",
            );

        let totals = compute_file_size(
            &fixture.context(),
            &[SizedImport::installed(
                request("consumer-lib"),
                Some(result("consumer-lib", 10)),
            )],
        );

        assert!(totals.error.is_none(), "{:?}", totals.diagnostics);
        assert!(!totals.degraded, "the combined build succeeded");
        assert!(
            totals.raw_bytes > 0,
            "the graph that did bundle is measured"
        );
        assert!(
            totals.incomplete,
            "the bytes behind the boundary are absent from the total: {:?}",
            totals.diagnostics
        );
        assert!(!totals.is_file_cost());
    }

    /// The native-binary-only twin of the check above: a `bin`-only package (Biome) is answered
    /// Measured at zero and must leave the file complete.
    #[test]
    fn a_native_binary_only_import_is_a_measurement_and_leaves_its_file_complete() {
        let fixture = Fixture::new("native-binary-only");
        fixture
            .package("real-lib", "export const value = 41 + 1;\n")
            .native_binary_only_package("native-lib");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                SizedImport::installed(request("real-lib"), Some(result("real-lib", 10))),
                SizedImport::installed(
                    request("native-lib"),
                    Some(native_binary_only_result("native-lib")),
                ),
            ],
        );

        assert!(
            totals.error.is_none(),
            "the real package builds; nothing failed: {:?}",
            totals.diagnostics
        );
        assert!(
            !totals.incomplete,
            "a native-binary-only import contributes a genuine ZERO, not an unknown: {:?}",
            totals.diagnostics
        );
        assert!(!totals.degraded, "the combined build succeeded");
        assert!(
            totals.is_file_cost(),
            "a file whose only unresolvable import is native-binary-only is fully measured, and \
             must be cached"
        );
        assert!(totals.raw_bytes > 0, "the real package's bytes are counted");
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|item| item.stage == crate::pipeline::stage::NATIVE_BINARY_ONLY),
            "the user is still told why that import contributes nothing: {:?}",
            totals.diagnostics
        );
    }

    /// ADR-0006, invariant 4, first bullet: if the combined build succeeds, the total is real even
    /// while every per-import result is still Loading.
    ///
    /// A File Cost has its own build over all the file's imports, independent of the per-import
    /// builds, so a cold document (every `result` still `None`) has a genuine total that must be
    /// cached. Flagging a Loading contributor (`if import.result.is_none() { totals.incomplete =
    /// true; }` in `compute_file_size_with`) would make every cold document a floor. Only the
    /// fallback sum in `per_import_totals` treats `None` as a missing input, because no build is
    /// left there to count the bytes.
    #[test]
    fn a_cold_document_whose_combined_build_succeeds_is_not_a_floor() {
        let fixture = Fixture::new("cold");
        fixture
            .package("alpha-lib", "export const alpha = 1;\n")
            .package("beta-lib", "export const beta = 2;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                // Nothing is measured yet; the combined build still runs and answers.
                SizedImport::installed(request("alpha-lib"), None),
                SizedImport::installed(request("beta-lib"), None),
            ],
        );

        assert!(
            totals.error.is_none(),
            "test setup: the combined build succeeds — both packages are real: {:?}",
            totals.diagnostics
        );
        assert!(
            !totals.degraded,
            "test setup: the file's own build succeeded, so the totals are a real File Cost: {:?}",
            totals.diagnostics
        );
        assert!(
            totals.raw_bytes > 0 && totals.minified_bytes > 0,
            "test setup: the combined build produced the file's bytes: {totals:?}"
        );
        assert!(
            !totals.incomplete,
            "a Loading contributor is NOT a missing input when the file's own combined build \
             succeeded — that build counted its bytes. Flagging it makes every cold document a \
             permanent floor, which is the regression ADR-0006 invariant 4 records: {:?}",
            totals.diagnostics
        );
        assert!(
            totals.is_file_cost(),
            "and a cold document's total must be CACHED, or the combined build re-runs on every \
             keystroke and `importlens check` can never judge a file it measured first: {:?}",
            totals.diagnostics
        );
    }

    #[test]
    fn file_cost_counts_a_font_shared_by_two_stylesheets_once() {
        const FONT_BYTES: usize = 6 * 1024;

        let fixture = Fixture::new("shared-css-font");
        fixture
            .package(
                "font-lib",
                "import './first.css';\nimport './second.css';\nexport const value = 42;\n",
            )
            .package_file(
                "font-lib",
                "package.json",
                r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.css"]}"#,
            )
            .package_file(
                "font-lib",
                "first.css",
                "@font-face { font-family: First; src: url('./shared.woff2'); }\n",
            )
            .package_file(
                "font-lib",
                "second.css",
                "@font-face { font-family: Second; src: url('./shared.woff2'); }\n",
            )
            .package_file("font-lib", "shared.woff2", []);

        let imports = [SizedImport::installed(request("font-lib"), None)];
        let empty_font = compute_file_size(&fixture.context(), &imports);
        fixture.package_file("font-lib", "shared.woff2", vec![0x6d; FONT_BYTES]);
        let populated_font = compute_file_size(&fixture.context(), &imports);

        for totals in [&empty_font, &populated_font] {
            assert!(totals.error.is_none(), "{totals:?}");
            assert!(
                !totals.degraded,
                "the combined build must succeed: {totals:?}"
            );
            assert!(
                !totals.incomplete,
                "every emitted file is readable: {totals:?}"
            );
        }
        assert_eq!(
            populated_font.raw_bytes.checked_sub(empty_font.raw_bytes),
            Some(FONT_BYTES as u64),
            "zero means the CSS font was omitted; twice the font length means its two references \
             were counted twice"
        );
        assert_eq!(
            populated_font
                .minified_bytes
                .checked_sub(empty_font.minified_bytes),
            Some(FONT_BYTES as u64),
            "a binary artifact has no separate minification step"
        );
    }

    #[test]
    fn a_combined_build_that_omits_an_unparseable_stylesheet_is_a_floor() {
        let fixture = Fixture::new("uncounted-css-floor");
        fixture
            .package(
                "broken-css-lib",
                "import './broken.scss';\nexport const value = 42;\n",
            )
            .package_file(
                "broken-css-lib",
                "package.json",
                r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.scss"]}"#,
            )
            .package_file(
                "broken-css-lib",
                "broken.scss",
                "$brand: red;\n@mixin thing { color: $brand }\n.bad { @include thing }\n",
            );

        let totals = compute_file_size(
            &fixture.context(),
            &[SizedImport::installed(request("broken-css-lib"), None)],
        );

        assert!(
            totals.error.is_none(),
            "the JavaScript still built: {totals:?}"
        );
        assert!(
            !totals.degraded,
            "the combined build itself succeeded: {totals:?}"
        );
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage
                    == crate::engine::diagnostic_stage::UNCOUNTED_ASSETS),
            "the missing stylesheet bytes must be disclosed: {totals:?}"
        );
        assert!(
            totals.incomplete,
            "the JavaScript-only number is a floor when a shipped stylesheet is absent"
        );
        assert!(
            !totals.is_file_cost(),
            "the floor must not become File Cost history or a budget verdict"
        );
    }

    /// Every contributor Measured while the file's own combined build fails: `incomplete` is
    /// `false` and `error` is `None`, and only `degraded` says the total is an un-deduplicated
    /// over-count (ADR-0004).
    ///
    /// Deterministic (`parse`) on purpose: a timeout is also refused by the transient scan, but a
    /// durable stage is caught only by `degraded`.
    #[test]
    fn a_failed_combined_build_degrades_the_total_even_with_every_import_measured() {
        let fixture = Fixture::new("degraded");
        fixture
            .package("broken-lib", "export const oops = (;\n")
            .package("fine-lib", "export const fine = 1;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                SizedImport::installed(request("broken-lib"), Some(result("broken-lib", 100))),
                SizedImport::installed(request("fine-lib"), Some(result("fine-lib", 20))),
            ],
        );

        assert!(
            totals.degraded,
            "the file's own combined build failed, so these totals are not the file's: {:?}",
            totals.diagnostics
        );
        assert!(
            !totals.incomplete,
            "test setup: EVERY contributor is Measured, which is the whole point — a check that \
             only inspects the contributors sees nothing wrong here"
        );
        assert!(
            totals.error.is_none(),
            "test setup: the fallback sum succeeded, so `error` is None — the second thing a \
             consumer looks at, and the second thing that says nothing is wrong"
        );
        assert_eq!(
            totals.brotli_bytes, 120,
            "test setup: the number IS there — the un-deduplicated sum of the per-import costs"
        );
        assert!(
            !totals.is_file_cost(),
            "an un-deduplicated per-import sum is a different QUANTITY from a File Cost; caching \
             it serves a number the file never had for the whole TTL, and a budget judged against \
             it is neither passed nor failed (ADR-0006, invariants 4 and 5)"
        );
    }

    /// The minify-failure arm: the chunk linked but could not be minified, so the group falls back
    /// to the per-import sum. Every contributor is Measured and `error` is `None`, so only
    /// `degraded` says the number is wrong.
    ///
    /// The minifier is injected because no fixture can reach this arm (see
    /// `compute_file_size_with`); everything else is real.
    #[test]
    fn a_minify_failure_degrades_the_total_even_with_every_import_measured() {
        let fixture = Fixture::new("minify-degraded");
        fixture
            .package("alpha-lib", "export const alpha = 1;\n")
            .package("beta-lib", "export const beta = 2;\n");

        let totals = compute_file_size_with(
            &fixture.context(),
            &[
                SizedImport::installed(request("alpha-lib"), Some(result("alpha-lib", 100))),
                SizedImport::installed(request("beta-lib"), Some(result("beta-lib", 20))),
            ],
            &|_| Err("minifier gave up on the linked chunk".to_owned()),
            &real_compress,
        );

        assert!(
            totals.degraded,
            "the chunk could not be minified, so these totals are a per-import sum and not the \
             file's: {:?}",
            totals.diagnostics
        );
        assert!(
            !totals.incomplete,
            "test setup: EVERY contributor is Measured — `incomplete` sees nothing wrong here"
        );
        assert!(
            totals.error.is_none(),
            "test setup: the fallback sum succeeded, so `error` is None too: {:?}",
            totals.diagnostics
        );
        assert_eq!(
            totals.brotli_bytes, 120,
            "test setup: the number IS there — the un-deduplicated sum of the per-import costs"
        );
        assert!(
            !totals.is_file_cost(),
            "a per-import sum is a different QUANTITY from a File Cost (ADR-0004); caching it \
             serves a number the file never had for the whole TTL, and judging a budget against it \
             is neither a pass nor a fail (ADR-0006, invariants 4 and 5)"
        );
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|item| item.stage == crate::pipeline::stage::MINIFY),
            "the user is owed the stage that degraded the total: {:?}",
            totals.diagnostics
        );
    }

    /// Control for the injected minifier: with the real one, the same input is a clean, cacheable
    /// measurement, so degrading everything cannot satisfy the test above.
    #[test]
    fn the_same_file_with_a_working_minifier_is_a_clean_measurement() {
        let fixture = Fixture::new("minify-ok");
        fixture
            .package("alpha-lib", "export const alpha = 1;\n")
            .package("beta-lib", "export const beta = 2;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                SizedImport::installed(request("alpha-lib"), Some(result("alpha-lib", 100))),
                SizedImport::installed(request("beta-lib"), Some(result("beta-lib", 20))),
            ],
        );

        assert!(!totals.degraded, "{:?}", totals.diagnostics);
        assert!(!totals.incomplete, "{:?}", totals.diagnostics);
        assert!(totals.is_file_cost(), "{:?}", totals.diagnostics);
        assert!(totals.minified_bytes > 0);
    }

    /// Compression runs per runtime (ADR-0005), so a compressor failure must degrade only its own
    /// group, like the build and minify arms: an early `return error_computation(..)` inside the
    /// loop would discard every other group's real bytes and report zero for the file.
    ///
    /// The compressor is injected (no fixture can make `compress_all` fail) and fails only the
    /// Server group, selected by a marker string the minifier preserves.
    #[test]
    fn a_compression_failure_in_one_runtime_does_not_zero_the_file() {
        let fixture = Fixture::new("compress-degraded");
        fixture
            .package("server-lib", "export const value = \"MARKER_SERVER\";\n")
            .package("client-lib", "export const value = \"MARKER_CLIENT\";\n");

        let clean = compute_file_size(
            &fixture.context(),
            &[SizedImport::installed(
                runtime_request("client-lib", ImportRuntime::Client),
                Some(result("client-lib", 20)),
            )],
        );
        assert!(
            clean.brotli_bytes > 0,
            "test setup: the Client group alone compresses to real bytes: {:?}",
            clean.diagnostics
        );

        let totals = compute_file_size_with(
            &fixture.context(),
            &[
                SizedImport::installed(
                    runtime_request("server-lib", ImportRuntime::Server),
                    Some(result("server-lib", 100)),
                ),
                SizedImport::installed(
                    runtime_request("client-lib", ImportRuntime::Client),
                    Some(result("client-lib", 20)),
                ),
            ],
            &minify_source,
            &|code| {
                if code.contains("MARKER_SERVER") {
                    return Err("compressor gave up on the Server chunk".to_owned());
                }
                real_compress(code)
            },
        );

        assert!(
            totals.error.is_none(),
            "one group's compressor failing is not a failure of the FILE: the Client group \
             compressed fine and the Server group has per-import measurements to fall back on: {:?}",
            totals.diagnostics
        );
        assert!(
            totals.degraded,
            "the Server group's totals are now an un-deduplicated per-import sum, so the file's \
             totals are not the file's: {:?}",
            totals.diagnostics
        );
        assert_eq!(
            totals.brotli_bytes,
            clean.brotli_bytes + 100,
            "the OTHER group's real compressed bytes must survive: the Client group keeps its {} \
             measured bytes and the Server group contributes its 100-byte per-import fallback. \
             Zero here is the regression: `return error_computation(..)` inside the loop throws \
             away every group that compressed cleanly.",
            clean.brotli_bytes,
        );
        assert!(
            !totals.is_file_cost(),
            "a part-bundle, part-per-import-sum total is not this file's size (ADR-0006, \
             invariant 4)"
        );
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|item| item.stage == crate::pipeline::stage::COMPRESSION),
            "the user is owed the stage that degraded the total: {:?}",
            totals.diagnostics
        );
    }

    /// A path alias is not a missing package: `@app/components` resolves to first-party source,
    /// which Import Lens does not measure (ADR-0004), so the total stays complete and cacheable.
    #[test]
    fn a_path_alias_import_leaves_its_file_complete_and_cacheable() {
        let fixture = Fixture::new("path-alias");
        fixture.package("fine-lib", "export const fine = 1;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                SizedImport::installed(request("fine-lib"), Some(result("fine-lib", 20))),
                SizedImport::path_alias("@app/components"),
            ],
        );

        assert!(
            !totals.incomplete,
            "an alias is not an unmeasured dependency: {:?}",
            totals.diagnostics
        );
        assert!(
            totals.is_file_cost(),
            "aliased files must still be cached and persisted, or the combined build re-runs on \
             every keystroke and `importlens check` exits 3 forever: {:?}",
            totals.diagnostics
        );
        assert!(totals.raw_bytes > 0, "the real package is still measured");
        assert!(
            totals
                .diagnostics
                .iter()
                .any(|item| item.stage == crate::pipeline::stage::PATH_ALIAS
                    && item
                        .details
                        .iter()
                        .any(|detail| detail == "specifier: @app/components")),
            "the user is still told why that specifier contributes nothing: {:?}",
            totals.diagnostics
        );
    }

    /// FR-024a, bullet 4: an import of a package that is **not installed** contributes no bytes and
    /// cannot become an entry of the combined build, so the total is a floor.
    #[test]
    fn a_not_installed_import_makes_the_total_a_floor() {
        let fixture = Fixture::new("not-installed");
        fixture.package("fine-lib", "export const fine = 1;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                SizedImport::installed(request("fine-lib"), Some(result("fine-lib", 20))),
                SizedImport::not_installed("ghost-lib"),
            ],
        );

        assert!(
            totals.incomplete,
            "an import whose package is not installed leaves the total short by its whole weight"
        );
        assert!(
            !totals.is_file_cost(),
            "a floor is never cached or persisted"
        );
        assert!(
            totals.diagnostics.iter().any(|item| item.stage
                == crate::pipeline::stage::PACKAGE_RESOLUTION
                && item
                    .details
                    .iter()
                    .any(|detail| detail == "specifier: ghost-lib")),
            "the user is owed the specifier that is missing: {:?}",
            totals.diagnostics
        );
    }

    /// An outright failure keeps the `incomplete` and `degraded` flags already raised, so the wire
    /// never says `incomplete: false` about a total known to be missing an import.
    #[test]
    fn an_outright_failure_keeps_the_floor_flag_it_had_already_raised() {
        let fixture = Fixture::new("error-flags");
        fixture.package("broken-lib", "export const oops = (;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                // Not installed → `incomplete` is raised BEFORE any build runs.
                SizedImport::not_installed("ghost-lib"),
                // Its build fails and it has no measurement to fall back on, so nothing is sized
                // and the aggregate fails outright.
                SizedImport::installed(request("broken-lib"), None),
            ],
        );

        assert!(
            totals.error.is_some(),
            "test setup: nothing could be sized, so the aggregate fails outright: {:?}",
            totals.diagnostics
        );
        assert!(
            totals.incomplete,
            "the floor flag was raised before the failure and must survive it onto the wire"
        );
        assert!(
            totals.degraded,
            "the combined build failed too, and that flag must survive as well"
        );
    }

    /// A first-party dependency's manifest (`sideEffects`, `exports`) moves this number without
    /// moving a module byte, and the extension's watcher globs only
    /// `**/node_modules/*/package.json`, so the manifest must be in the File Cost's freshness set.
    #[test]
    fn a_first_party_manifest_is_a_file_cost_freshness_input() {
        let workspace = std::env::temp_dir().join(format!(
            "il-file-cost-manifest-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        // A dependency whose entry escapes node_modules into workspace source: what a linked or
        // workspace package looks like once its paths are canonicalized.
        let linked = workspace.join("node_modules").join("linked");
        std::fs::create_dir_all(&linked).expect("linked package dir");
        std::fs::write(
            linked.join("package.json"),
            r#"{"name":"linked","version":"1.0.0","module":"index.js"}"#,
        )
        .expect("linked manifest");
        std::fs::write(
            linked.join("index.js"),
            "export * from \"../../packages/ui/src/index.js\";\n",
        )
        .expect("linked entry");

        let ui = workspace.join("packages").join("ui");
        std::fs::create_dir_all(ui.join("src")).expect("ui dirs");
        std::fs::write(
            ui.join("package.json"),
            r#"{"name":"ui","version":"1.0.0"}"#,
        )
        .expect("ui manifest");
        std::fs::write(
            ui.join("src").join("index.js"),
            "export const widget = () => 'widget';\n",
        )
        .expect("ui source");

        let computed = compute_file_size(
            &AnalysisContext {
                workspace_root: workspace.clone(),
                active_document_path: workspace.join("src").join("app.ts"),
            },
            &[SizedImport::installed(
                crate::ipc::protocol::ImportRequest {
                    specifier: "linked".to_owned(),
                    package_name: "linked".to_owned(),
                    version: "1.0.0".to_owned(),
                    named: vec!["widget".to_owned()],
                    import_kind: ImportKind::Named,
                    runtime: crate::ipc::protocol::ImportRuntime::Component,
                },
                None,
            )],
        );

        let has_manifest = computed.dependency_fingerprints.iter().any(|fingerprint| {
            fingerprint
                .path
                .replace('\\', "/")
                .ends_with("packages/ui/package.json")
        });
        assert!(
            has_manifest,
            "the first-party manifest must be a freshness input: {:?}",
            computed.dependency_fingerprints
        );

        std::fs::remove_dir_all(&workspace).ok();
    }
}
