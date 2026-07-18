use crate::{
    cache::key::{
        FileFingerprint, fingerprints_are_reusable, sort_and_dedup_fingerprints,
        unverifiable_file_fingerprint,
    },
    engine::{BundleEntry, BundlePurpose, BundleRequest, boundary},
    ipc::protocol::{
        ImportDiagnostic, ImportRequest, ImportResult, ImportRuntime, ModuleContribution,
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
/// The two non-`Installed` kinds are both "there is no `node_modules/<name>/package.json`", and they
/// are **not the same fact**. Collapsing them into one is a regression this enum exists to make
/// unrepresentable: see [`crate::pipeline::resolver::FirstPartySourceProbe`], which is what
/// decides between them — and decides it on positive evidence that the specifier IS first-party,
/// never on the absence of a `package.json` declaration.
#[derive(Debug, Clone)]
pub enum SizedPackage {
    /// Installed: the daemon resolved its manifest and built a request (a request carries the
    /// installed version), so this import is an **entry** of the file's combined build.
    Installed(ImportRequest),
    /// **Not installed** — and not first-party either: the specifier resolves to nothing at all. An
    /// uninstalled dependency, a typo, a stale import. Its bytes belong in this file's total and are
    /// missing from it, however cleanly every build goes, so the total is a floor (SRS FR-024a,
    /// bullet 4). Whether `package.json` happens to declare it changes none of that.
    NotInstalled,
    /// **Not a package at all** — a tsconfig path alias (`@app/components`, `~lib/foo`, or a bare
    /// `components/Button` under a `baseUrl`) resolving to first-party source. Import Lens measures
    /// third-party imports (ADR-0004), so first-party code contributes nothing to any total it
    /// reports, exactly like a relative import. It is **not a gap** and flags nothing: reading it as
    /// a missing dependency made every file that uses path aliases a permanent floor — never cached,
    /// never persisted, and refused a verdict by `importlens check`.
    ///
    /// (The `@/…`, `~/…`, `#…` and `$…` spellings never get this far: `document::specifier` drops
    /// them before detection. It is the alias forms that look like package names that reach here.)
    PathAlias,
}

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
/// A [`SizedPackage::NotInstalled`] import is not an entry of any build and has no measurement to
/// contribute, and it used to be dropped from the aggregate's input entirely (`service.rs` filtered
/// it out), so the file's total silently omitted it, was cached, was persisted as the file's
/// baseline, and passed `importlens check` with exit 0. It is a floor now, exactly like every other
/// unmeasured contributor (SRS FR-024a, bullet 4).
///
/// Carrying the measurement in rather than re-deriving it is also what keeps the fallback out of
/// the engine. It used to re-analyze every import of the failing runtime group from scratch, so
/// one combined build that parked cost a build timeout and then N more — duplicating, on a second
/// set of permits, the very builds the caller had already run or was already running.
#[derive(Debug, Clone)]
pub struct SizedImport {
    pub package: SizedPackage,
    /// The specifier as written in the source — the one thing every import has, installed or not.
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
    /// The file's **own combined build** failed, so these totals fell back to a sum of per-import
    /// costs — with no shared-module deduplication. That is a **different quantity** from a File
    /// Cost ([ADR-0004]): a module two imports both pull in is counted TWICE. It is an *over*-count,
    /// not a floor, and it is just as unusable: never cached, never persisted, never judged
    /// (ADR-0006, invariant 4, second half).
    ///
    /// **`incomplete` structurally cannot see this**, and that is the whole reason the flag exists.
    /// A combined build is strictly larger than any single import's build, which makes it the
    /// likeliest thing in the daemon to hit `BUILD_TIMEOUT` — and when it does, every one of the
    /// file's imports may still be perfectly Measured and cached. `missing_inputs` is then correctly
    /// `false`, `error` is `None`, every import on the wire carries a size, and the only trace of
    /// the failure is a `timeout` diagnostic that three of the four consumers never looked at.
    ///
    /// Set for a **deterministic** combined-build failure too. The previous fix reasoned that an
    /// over-count can never produce a false *pass*, so a budget verdict from it was safe. But a
    /// false FAIL is also a verdict, and invariant 5 forbids both: a budget judged against a number
    /// the file never had is neither passed nor failed.
    pub degraded: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Exact inputs of the combined build, retained only for the process-local File Cost cache.
    /// This is not part of the wire or disk schema.
    pub(crate) dependency_fingerprints: Vec<FileFingerprint>,
}

impl FileSizeComputation {
    /// Whether this aggregate is a measurement of the file, and so may be written to the L1
    /// file-size cache (SRS FR-026c). [`crate::pipeline::file_size_cache::FileSizeCache::insert`]
    /// asks this itself; a caller cannot forget it.
    ///
    /// Four ways it is not. It failed outright (`error`). It is [`Self::incomplete`] — a sum
    /// missing an input, which is a real number but not this file's. It is [`Self::degraded`] — the
    /// file's own combined build failed, so what is on offer is an un-deduplicated per-import sum,
    /// which is a real number and *also* not this file's. Or a **transient** stage rode in on a
    /// diagnostic some other way.
    ///
    /// `degraded` is not redundant with `incomplete`, and the two are not even the same *direction*
    /// of error: a floor is an under-count, a degraded total is an over-count. A combined build can
    /// park while every one of the file's imports is measured and cached, which leaves `incomplete`
    /// correctly `false` — and the totals still are not the file's.
    ///
    /// Nor is `degraded` redundant with the transient scan: it also catches the DETERMINISTIC
    /// combined-build failure, which carries a perfectly durable stage (`link`, `parse`) and would
    /// otherwise be cached for the whole 30s TTL and judged by CI.
    pub fn is_cacheable(&self) -> bool {
        self.error.is_none()
            && !self.incomplete
            && !self.degraded
            && fingerprints_are_reusable(&self.dependency_fingerprints)
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

/// How much of each import's weight another import of the SAME document also pulls in — counted
/// **within a runtime**, because that is the only place a shared module is shared.
///
/// Each import arrives paired with the runtime it resolves under, and the count is partitioned by
/// it. A module reached from Astro frontmatter (Server) and from a client `<script>` (Client) is
/// **not** shared: the two runtimes are two artifacts that ship separately, each carrying its own
/// copy ([ADR-0005]). Counting it across the boundary claimed a deduplication the build model
/// explicitly does not perform, and `insights.ts` rendered that claim to the user as a
/// shared-dependency saving — on exactly the file shape the runtime split exists to handle.
///
/// The runtime comes in with the result rather than being re-derived here, so there is exactly one
/// source of the partition.
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
/// A `BundleRequest` carries a single runtime, and Rolldown resolves the whole
/// transitive graph under it. Root entries are pre-resolved per request, so their
/// own paths are always right — but Server and Client resolve dependencies under
/// materially different conditions (`browser` alias fields, `browser` vs `node`
/// export conditions). Sizing every entry under one import's runtime therefore
/// resolves the *other* runtime's packages against the wrong conditions, and the
/// mis-conditioned build still succeeds, so nothing warns. A single Astro file
/// reaches this: frontmatter imports are Server, processed `<script>` imports are
/// Client (design doc §6.3, I15).
///
/// Grouping is per runtime rather than per entry on purpose: shared-module
/// deduplication is only ever real *within* a runtime, since Server and Client code
/// never share a chunk in the shipped product.
///
/// Each group is minified and **compressed on its own, and the results are added** — a runtime is
/// an artifact boundary, and compressed bytes may be summed across such a boundary and never within
/// one ([ADR-0005], [ADR-0004]; design doc §6.3, I20/I15). Only a document that mixes runtimes has
/// more than one group, and only an Astro document mixes them: every other document's imports are
/// `Component`, so the sum has exactly one term and cannot over-report.
pub fn compute_file_size(
    context: &AnalysisContext,
    imports: &[SizedImport],
) -> FileSizeComputation {
    compute_file_size_with(
        context,
        imports,
        &|code| minify_source(code, false),
        &|code| compress_all(code).map_err(|error| error.to_string()),
    )
}

/// [`compute_file_size`] with the minifier injected, which no production caller does.
///
/// The minify-failure arm below degrades the file's totals exactly as a build failure does, and
/// **no fixture can reach it**: Rolldown parses every module with the same OXC parser that
/// [`minify_source`] re-parses the linked chunk with, in strict module mode, so any source that
/// would fail the chunk's re-parse fails the *build* first. (Measured, not assumed: `'0'`-prefixed
/// octal literals, `with`, duplicate parameters, `delete` of a local, a hashbang — every one of them
/// comes back a `parse` failure of the combined build, never a `minify` failure of its chunk.)
///
/// The arm is real all the same — a codegen/minifier defect, or a construct OXC can print and not
/// re-parse — and being unreachable from a fixture is precisely why it went untested: `degraded` was
/// deleted from it and the entire daemon suite stayed green, which is Critical 1's exact shape (the
/// file's own build fails while every contributor is Measured). So the seam is here, used by one
/// test, and by nothing else.
fn compute_file_size_with(
    context: &AnalysisContext,
    imports: &[SizedImport],
    minify: &dyn Fn(&str) -> Result<String, String>,
    compress: &dyn Fn(&str) -> Result<CompressionSizes, String>,
) -> FileSizeComputation {
    let mut diagnostics = Vec::new();
    let mut totals = FileSizeComputation::default();
    // Entries and their originating imports, grouped by the runtime they must be
    // built under. `BTreeMap` keeps the group order stable so identical input
    // produces identical output.
    let mut groups: BTreeMap<ImportRuntime, RuntimeGroup> = BTreeMap::new();

    for import in imports {
        let specifier = format!("specifier: {}", import.specifier);
        let request = match &import.package {
            SizedPackage::Installed(request) => request,
            SizedPackage::NotInstalled => {
                // The package is NOT INSTALLED and the specifier is not first-party source: no
                // request, no entry, no measurement. Its bytes are missing from the totals however
                // cleanly every build goes, and it used to be filtered out of the aggregate's input
                // before it could say so (SRS FR-024a, bullet 4). Floor.
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
                // NOT a missing dependency: a tsconfig path alias, which RESOLVES to first-party
                // source. Import Lens measures third-party imports (ADR-0004), so this contributes
                // nothing to a total it reports — exactly like a relative import, which is never even
                // detected. It is a fact, not a gap: NO flag, and the total stays complete. Flagging
                // it made every aliased file a permanent floor.
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
            // A **declarations-only** package resolves to `Err` BY DESIGN — it ships no runtime
            // entry because it ships no runtime code — and `pipeline::types_only` answers it
            // MEASURED: a genuine zero, at High confidence. It is not an entry of any build and it
            // contributes no bytes, and *both of those are facts*, so the total stays complete.
            //
            // Treating it as a gap (which the resolution check briefly did) made every file that
            // imports an `@types/…` or any declarations-only package a permanent floor: the combined
            // build re-ran on every size request, nothing was ever cached or persisted, and
            // `importlens check` exited 3 — for a large fraction of real TypeScript files.
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
            // A **native-binary-only** package resolves to `Err` for the same reason — it ships no
            // importable JS entry — and `pipeline::native_binary` answers it MEASURED at zero. It is
            // not an entry of any build and contributes no bytes, both facts, so the total stays
            // complete rather than becoming a floor.
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
                // This import is not an ENTRY of any group, so its bytes are missing from the
                // totals however cleanly the combined builds go — the one non-Measured contributor
                // a successful build cannot absorb. Floor (ADR-0006, invariant 4).
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
        // No combined build to run. Either the file has no imports at all, or every one of them is
        // declarations-only or a path alias (all three a complete, honest zero), or not one could be
        // resolved (`incomplete`, and never cached as this file's size).
        return FileSizeComputation {
            diagnostics,
            ..totals
        };
    }

    // Each runtime group is minified AND COMPRESSED on its own, and the results are added.
    //
    // A runtime is an artifact boundary (ADR-0005): the Server bundle and the Client bundle are two
    // things that ship, each carrying its own copy of anything both need, and each genuinely
    // compressed alone. Summing their separately-compressed sizes therefore models reality exactly.
    //
    // This used to join the groups' minified outputs and compress the CONCATENATION once, on the
    // reasoning that summing separately-compressed parts is unsound because compression is not
    // additive. Non-additivity is real, but it applies to parts that would in reality be compressed
    // TOGETHER — and two runtime groups never are. The join compressed away every byte of
    // redundancy between two payloads that never meet, so the figure it produced was a strict lower
    // bound on what ships (measured at ~49% under-report on a shared-heavy two-runtime Astro file),
    // presented as a size, and about to gate the per-file budget. See the design doc §6.3 (I20,
    // superseding I15's first accepted consequence).
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
                // shared-module-deduplicated numbers rather than being discarded with them — but
                // the FILE's totals are now part deduplicated bundle, part per-import sum, so they
                // are not the file's either way. `degraded` says so, for ANY failure stage: a
                // timeout is the likeliest cause (this is the biggest build in the system) and a
                // deterministic link failure is the one that reads as durable and gets cached.
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

        // `record_loaded_paths` is deliberately NOT called here. This build's
        // `loaded_paths` is the union over every entry in the group, and writing that
        // union under each entry's key would clobber the accurate per-entry sets the
        // per-import analyses already recorded (`analyze.rs`), making an edit to one
        // package invalidate another document's cached size for an unrelated one
        // (design doc §6.3, I14).
        diagnostics.extend(artifact.diagnostics.iter().map(|item| ImportDiagnostic {
            stage: item.stage.clone(),
            message: item.message.clone(),
            details: Vec::new(),
        }));

        let minified = match minify(&artifact.code) {
            Ok(minified) => minified,
            Err(error) => {
                // Degrade only this runtime, exactly as a build failure does. Returning
                // here would discard every other group's real totals and report zero
                // for the whole file. The chunk linked but could not be minified, so this
                // group's contribution falls back to the same un-deduplicated per-import sum.
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
                // Degrade only this runtime, exactly as the build and minify arms above do. This
                // arm used to sit AFTER the loop, where `return error_computation(..)` was right:
                // there was one compression pass over every group's output, so its failure really
                // was the file's. Inside the loop that same `return` discards every OTHER group's
                // real, already-measured bytes and reports ZERO for the whole file — one group's
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

        // This group's non-JavaScript assets, processed the way they ship (B2). The combined build
        // saw every import in this runtime, so its stylesheets bundle into ONE artifact for the
        // whole group — which is how they ship, and it dedupes what two imports both `@import`
        // rather than counting it twice. Each artifact is compressed on its own and summed
        // (ADR-0005); an asset that cannot be processed falls back to disclosure and is reported.
        let assets = match process_assets_bounded(
            artifact.assets.clone(),
            artifact.graph_source_bytes,
            artifact.loaded_paths.clone(),
        ) {
            Ok(assets) => assets,
            Err(failure) => {
                // The JS chunk completed, but the runtime group's asset tail did not produce one
                // coherent measurement. Degrade this group exactly like a combined-build failure:
                // keep the already-known per-import floor and never cache or judge it as File Cost.
                totals.degraded = true;
                totals
                    .dependency_fingerprints
                    .extend(artifact.read_time_fingerprints.iter().cloned());
                totals
                    .dependency_fingerprints
                    .extend(failure.read_time_fingerprints);
                totals.dependency_fingerprints.extend(
                    artifact
                        .unhashed_paths
                        .iter()
                        .map(unverifiable_file_fingerprint),
                );
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
        totals
            .dependency_fingerprints
            .extend(artifact.read_time_fingerprints.iter().cloned());
        totals
            .dependency_fingerprints
            .extend(assets.freshness_fingerprints());
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

        any_sized = true;
        totals.raw_bytes += artifact.code.len() as u64 + asset_sizes.raw_bytes;
        // `minified_bytes` is measured on the same string this group's compressors saw, so the two
        // numbers describe the same bytes. The old join added one separator per extra group, so the
        // minified total described a string that ships nowhere.
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
/// `FileSizeComputation` cannot fail when the fold is wrong, and the fold is where the *first* half
/// of ADR-0006 §4 lives: it is what decides whether an import that was never measured leaves a mark
/// on the total.
///
/// **It is structurally blind to the second half, and that is how the second half survived.** This
/// helper starts from a `FileSizeComputation::default()` and never runs a combined build, so
/// `degraded` is always `false` here — the one shape where every contributor is Measured and the
/// aggregate is still not the file's size cannot be expressed through it at all. Only
/// [`compute_file_size`] can see that, so the tests for it go through `compute_file_size`.
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
/// It carries the flags forward rather than starting from `default()`, which silently reset
/// `incomplete` to `false` — so the wire could say `incomplete: false` about a total that was
/// already known to be missing an import before the failure that zeroed it. Every gate refuses this
/// shape on `error` alone, so nothing was mis-stored, but the client was told something untrue and
/// the next person to read `incomplete` in isolation would have believed it.
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

    /// The shape a TIMEOUT/PANIC leaves behind now: Unmeasured. No size at all — not the entry
    /// file measured alone, not a zero.
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
    fn a_sum_of_real_measurements_is_the_file_and_is_cacheable() {
        let totals = absorb(&[measured("alpha", 100), measured("beta", 20)]);

        assert_eq!(totals.raw_bytes, 120);
        assert!(!totals.incomplete);
        assert!(
            totals.is_cacheable(),
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
            !totals.is_cacheable(),
            "no on-disk file can validate two different known snapshots of one path"
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
            SizedImport::installed(request("beta"), None),
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

    // ---------------------------------------------------------------------------------------
    // Through `compute_file_size` itself.
    //
    // Everything above routes through `per_import_totals_for_test`, which never runs a combined
    // build — so `degraded` is always false there and the shape ADR-0006 §4's second half names is
    // literally not expressible. That is exactly how it survived seven rounds of review. These go
    // through the real entry point, on a real fixture, with a real Rolldown build.
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
        /// Rolldown build at `parse` — deterministically, which is the half of invariant 4 the
        /// previous fix declined to act on.
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

    /// **CRITICAL 2 — the regression.** Re-resolving every import inside `compute_file_size` and
    /// flagging `incomplete` on any `Err` treats a declarations-only package as an unmeasured gap.
    /// It is not: it resolves to nothing BECAUSE it ships nothing, and it is answered Measured. A
    /// file importing `@types/…` would otherwise carry an `incomplete` total forever — never cached,
    /// never persisted, exit 3 from `importlens check`.
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
            totals.is_cacheable(),
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

    /// The native-binary-only twin of the check above. A `bin`-only package (Biome) resolves to
    /// `Err` because it ships no importable JS entry, but it is answered Measured at zero, so it
    /// contributes a genuine ZERO and must leave the file complete — not a permanent floor.
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
            totals.is_cacheable(),
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

    /// **ADR-0006, invariant 4, first bullet — and it had NO test at all.**
    ///
    /// *"If the combined build SUCCEEDS, the total is real — even while every per-import result is
    /// still Loading. On a cold document that is the normal case, and it is not a floor."*
    ///
    /// A File Cost has its **own build**: one bundle over all the file's imports, which does not
    /// depend on the per-import builds at all. So a document nobody has measured yet — every
    /// `result` still `None`, because the streaming handlers answer from cache and let the misses
    /// land later — has a total that is a genuine measurement of the file, and it must be cached. On
    /// a cold document that is the NORMAL case.
    ///
    /// The ADR records that reading it the other way already caused a regression once: flagging any
    /// Loading contributor made **every cold document** a floor, so nothing was ever cached and the
    /// combined build re-ran on every keystroke. Yet the mutation that reintroduces it —
    /// `if import.result.is_none() { totals.incomplete = true; }` at the top of the loop in
    /// `compute_file_size_with` — left the entire daemon suite green (162 lib, 49 service, 500
    /// total, 0 failed). Every existing test either measures its imports or fails the build. An
    /// invariant nothing can detect is not an invariant.
    ///
    /// The distinction this pins down is exactly the one `per_import_totals` gets right for the
    /// FALLBACK sum, where a `None` result really is a missing input: there, no build is left to
    /// count the bytes. Here the build counted them.
    #[test]
    fn a_cold_document_whose_combined_build_succeeds_is_not_a_floor() {
        let fixture = Fixture::new("cold");
        fixture
            .package("alpha-lib", "export const alpha = 1;\n")
            .package("beta-lib", "export const beta = 2;\n");

        let totals = compute_file_size(
            &fixture.context(),
            &[
                // NOTHING is measured: this is a cold document, and the per-import builds have not
                // landed. The combined build still runs, and still answers.
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
            totals.is_cacheable(),
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

    /// **CRITICAL 1.** Every contributor Measured, and the file's OWN combined build fails. The
    /// contributors are all fine, so `incomplete` is correctly `false`; `error` is `None`, because
    /// the fallback summed successfully. What is on the wire is an un-deduplicated per-import sum —
    /// a Combined Import Cost (ADR-0004), an OVER-count, a number the file never had — and the only
    /// thing that says so is `degraded`.
    ///
    /// Deterministic (`parse`) on purpose: the timeout case was already refused by the transient
    /// scan, and this one was not. It carries a durable stage, so it was cached for the L1 TTL,
    /// persisted as the file's baseline, and judged by `importlens check`.
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
            !totals.is_cacheable(),
            "an un-deduplicated per-import sum is a different QUANTITY from a File Cost; caching \
             it serves a number the file never had for the whole TTL, and a budget judged against \
             it is neither passed nor failed (ADR-0006, invariants 4 and 5)"
        );
    }

    /// **The minify-failure arm.** The chunk LINKED — the combined build succeeded — and the
    /// minifier could not process it, so this runtime group falls back to the same un-deduplicated
    /// per-import sum a build failure falls back to, and the file's totals are just as much not the
    /// file's. Every contributor here is Measured and `error` is `None`, which is Critical 1's exact
    /// shape: nothing but `degraded` says the number is wrong.
    ///
    /// `degraded` was deleted from this arm and the entire daemon suite stayed green.
    ///
    /// The minifier is injected because **no fixture can reach this arm** — Rolldown parses each
    /// module with the same OXC parser that re-parses the linked chunk, so anything that would fail
    /// the chunk's re-parse fails the *build* first (measured: octal literals, `with`, duplicate
    /// parameters, `delete` of a local, a hashbang — every one comes back a `parse` failure of the
    /// build). Being unreachable from a fixture is exactly why the arm went untested; it is not a
    /// reason to leave it that way. Everything else in this test is the real thing: a real package,
    /// a real Rolldown build, the real fallback.
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
            !totals.is_cacheable(),
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

    /// Control for the injected minifier: with the REAL one, the same input is a clean, cacheable
    /// measurement. Without this, the test above could be "made to pass" by degrading everything.
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
        assert!(totals.is_cacheable(), "{:?}", totals.diagnostics);
        assert!(totals.minified_bytes > 0);
    }

    /// **The trap in moving compression inside the per-runtime loop.** Compression is now per
    /// runtime, because two runtimes are two artifacts that ship separately and each pays for its
    /// own redundancy (ADR-0005). The naive move takes the old post-loop `Err` arm with it — a
    /// `return error_computation(..)` — and that arm now fires **inside** the loop, discarding every
    /// OTHER runtime group's real, already-measured bytes and reporting **zero** for the whole file.
    ///
    /// One group's compressor failing says nothing about the other group's bytes. So it degrades
    /// exactly like the build and minify arms beside it: that group alone falls back to its
    /// un-deduplicated per-import sum, the file is flagged `degraded` and refused every store, and
    /// the other groups keep their real numbers.
    ///
    /// The compressor is injected because nothing in a fixture can make `compress_all` fail — the
    /// same reason the minify arm is injected. It selects the Server group by a marker string the
    /// minifier preserves, so exactly one of the two groups fails.
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
            &|code| minify_source(code, false),
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
            !totals.is_cacheable(),
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

    /// **IMPORTANT 1 — a path alias is not a missing package.** `@app/components` has no installed
    /// package and no request, exactly like an uninstalled dependency, and it is a completely
    /// different fact: it resolves to first-party source, which Import Lens does not measure
    /// (ADR-0004). It contributes no bytes because there are none to contribute — a fact, not a gap
    /// — so the total stays complete and cacheable. Flagging it made every file that uses path
    /// aliases a permanent floor.
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
            totals.is_cacheable(),
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
    /// cannot even become an entry of the combined build. It used to be filtered out of the
    /// aggregate's input before it could say so, and the file's total silently omitted it.
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
            !totals.is_cacheable(),
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

    /// The MINOR: `error_computation` rebuilt the result from `FileSizeComputation::default()`,
    /// which reset `incomplete` to `false` — so the wire carried `incomplete: false` on a total
    /// already known to be missing an import. Nothing mis-stored it (every gate refuses on `error`),
    /// but the client was told something untrue.
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
}
