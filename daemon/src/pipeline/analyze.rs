use crate::{
    engine::{dependency_paths::record_loaded_paths, limits::MAX_MODULE_SOURCE_BYTES},
    ipc::protocol::{
        ConfidenceLevel, ImportDiagnostic, ImportKind, ImportRequest, ImportResult, MeasuredSizes,
        ModuleContribution,
    },
    pipeline::{
        assets::{asset_diagnostics, process_assets},
        compress::compress_all,
        fallback::source_excerpt_detail,
        full_package,
        minify::minify_source,
        native_binary::{annotate_native_binary, native_binary_only_package_result},
        resolver::{ResolvedPackage, SideEffectsMode, resolve_package_entry},
        types_only::declaration_only_package_result,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Where an analysis runs, and nothing else.
///
/// It used to carry an engine deadline too, because the response an import rode in was atomic:
/// one build that parked pushed a whole document's results past the client's patience, so the
/// request had to be able to abandon builds. It no longer is — a request answers from cache and
/// each build is pushed to the client as it lands (`ipc::server`) — so no build has a deadline
/// to be measured against, and none is passed one.
#[derive(Debug, Clone)]
pub struct AnalysisContext {
    pub workspace_root: PathBuf,
    pub active_document_path: PathBuf,
}

// Internal structured error translated into the stable ImportResult surface.
#[derive(Debug, Clone)]
pub struct AnalysisError {
    stage: &'static str,
    message: String,
    details: Vec<String>,
    /// Fingerprints of every module the failing build READ — empty for a failure that never
    /// entered the engine.
    ///
    /// A DETERMINISTIC failure is cached (ADR-0006, invariant 3), and a cached fact must expire
    /// exactly when the fact would change. Fingerprinting only the entry and the manifest does not
    /// promise that: a workspace package whose entry merely re-exports the module that fails to
    /// parse would keep serving the cached failure after the user fixed it, because nothing the
    /// cache watches moved. So the failure is fingerprinted against the bytes it was derived from,
    /// exactly as a success is.
    read_time_fingerprints: Vec<crate::cache::key::FileFingerprint>,
    /// The modules that parsed before the build gave up, used only to find the first-party
    /// manifests that shaped the resolution.
    loaded_paths: Vec<PathBuf>,
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
    analyze_resolved_import_with_dependencies(context, request, resolved).0
}

/// Manifests of the first-party packages whose sources this build loaded (§8.3).
///
/// The plugin records graph *modules*, and a `package.json` is never one — but it
/// drives resolution and side-effect classification, so editing a workspace
/// dependency's `exports`, `type` or `sideEffects` changes what the bundler pulls in
/// while no fingerprinted path moves, and a stale size is served as fresh. The dep's
/// *source* files are fingerprinted, so editing its code is already caught; what is
/// missed is editing its manifest.
///
/// Installed packages are excluded: their manifests cannot change without an install,
/// which bumps the cache generation, and including them would balloon the fingerprint
/// set for every build.
fn first_party_manifests(context: &AnalysisContext, loaded_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Loaded paths are canonicalized (verbatim `\\?\C:\...` on Windows) while the
    // workspace root arrives off the wire as the editor spelled it, so the two never
    // compare equal unless the root is canonicalized too — and the walk below would
    // not actually stop where this says it does.
    let workspace_root = fs::canonicalize(&context.workspace_root)
        .unwrap_or_else(|_| context.workspace_root.clone());

    for path in loaded_paths {
        if path
            .components()
            .any(|component| component.as_os_str() == "node_modules")
        {
            continue;
        }

        // Nearest manifest at or above the module, bounded by the workspace root so a
        // loose file outside the workspace cannot walk to the filesystem root.
        let mut directory = path.parent();
        while let Some(current) = directory {
            if seen.contains(current) {
                break;
            }
            seen.insert(current.to_path_buf());

            let manifest = current.join("package.json");
            if manifest.is_file() {
                manifests.push(manifest);
                break;
            }
            if current == workspace_root {
                break;
            }
            directory = current.parent();
        }
    }

    manifests
}

/// Everything the full-package memo must expire against: the read-time fingerprints
/// of every module the comparison build measured, plus the manifests that decide what
/// it resolved. Mirrors the freshness set the import cache stores for the entry build
/// itself — if the two ever diverge, the memo would outlive the size it describes.
fn full_package_fingerprints(
    context: &AnalysisContext,
    package_root: &Path,
    full: &crate::engine::BundleArtifact,
) -> Vec<crate::cache::key::FileFingerprint> {
    manifest_augmented_fingerprints(
        context,
        package_root,
        &full.read_time_fingerprints,
        &full.loaded_paths,
    )
}

/// A build's read-time fingerprints, augmented with the package's own manifest and the
/// first-party manifests its sources loaded (§8.3) — the freshness set every build-derived
/// memo needs so a manifest edit that no source file reflects still expires it.
///
/// Shared by the full-package comparison memo and the export-list memo so the two cannot
/// drift in what they call "still fresh" — and so there is exactly one manifest walker
/// (`first_party_manifests`), per ADR-0002.
pub(crate) fn manifest_augmented_fingerprints(
    context: &AnalysisContext,
    package_root: &Path,
    read_time_fingerprints: &[crate::cache::key::FileFingerprint],
    loaded_paths: &[PathBuf],
) -> Vec<crate::cache::key::FileFingerprint> {
    use crate::cache::key::file_fingerprint_reading_hash;

    let mut fingerprints = read_time_fingerprints.to_vec();
    fingerprints.extend(
        std::iter::once(package_root.join("package.json"))
            .chain(first_party_manifests(context, loaded_paths))
            .filter_map(file_fingerprint_reading_hash),
    );
    fingerprints
}

/// Freshness inputs for a successful engine build (§8.3).
///
/// `fingerprints` were captured as each module's bytes were read *during* the
/// build, so they describe exactly the bytes the size was measured from. Anything
/// that is not a graph module — the package manifest, and binary modules the plugin
/// handed back to Rolldown — has no read-time capture and is listed in `stat_paths`
/// for the caller to hash.
pub enum FingerprintSource {
    ReadTime {
        fingerprints: Vec<crate::cache::key::FileFingerprint>,
        stat_paths: Vec<PathBuf>,
    },
}

/// Analyze a pre-resolved entry and return the exact real paths Rolldown loaded.
pub fn analyze_resolved_import_with_dependencies(
    context: &AnalysisContext,
    request: &ImportRequest,
    resolved: ResolvedPackage,
) -> (ImportResult, Option<FingerprintSource>) {
    match analyze_import_inner_resolved(context, request, resolved) {
        Ok((result, source)) => (result, source),
        Err(error) => {
            // A deterministic failure is CACHED (ADR-0006), so it must expire when the answer would
            // change — which means fingerprinting the bytes the failure was derived from, not just
            // the entry the caller happened to name. The engine reports what it had loaded when it
            // gave up; those are those bytes.
            let source = engine_failure_fingerprints(context, request, &error);
            (error_result(request, error), source)
        }
    }
}

/// Freshness inputs for a FAILED engine build: the bytes it read, plus the manifests that decided
/// what it resolved. Mirrors the success path's set (`analyze_with_rolldown_engine`) exactly,
/// because the requirement is exactly the same — the cached answer must expire when the bytes it
/// was derived from change.
///
/// `None` for a failure that never reached the engine — an unreadable manifest, an unresolvable
/// entry, an oversized entry. Those have no graph, and `service::dependency_fingerprints` falls
/// back to the entry and the manifest, which for those three IS the set that would have to change
/// for the answer to change.
fn engine_failure_fingerprints(
    context: &AnalysisContext,
    request: &ImportRequest,
    error: &AnalysisError,
) -> Option<FingerprintSource> {
    if error.read_time_fingerprints.is_empty() {
        return None;
    }

    let mut stat_paths = Vec::new();
    if let Ok(resolved) = resolve_package_entry(&context.active_document_path, request) {
        stat_paths.push(resolved.package_root.join("package.json"));
    }
    stat_paths.extend(first_party_manifests(context, &error.loaded_paths));

    Some(FingerprintSource::ReadTime {
        fingerprints: error.read_time_fingerprints.clone(),
        stat_paths,
    })
}

fn analyze_import_inner(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ImportResult, AnalysisError> {
    // No arm here invents a size. A manifest that cannot be read used to be answered with the
    // package directory's bytes ON DISK — unminified, uncompressed, tests, source maps and all —
    // and that one number was assigned to all five size fields, so the "brotli" size of such an
    // import was an uncompressed directory. It is Unmeasured now (ADR-0006).
    let resolved = match resolve_import_package(context, request) {
        Ok(resolved) => resolved,
        Err(error) if error.stage == crate::pipeline::stage::ENTRY_RESOLUTION => {
            // A declarations-only package is MEASURED, not Unmeasured: it really does ship zero
            // runtime bytes. Its diagnostic stage is what keeps `Some(0)` unambiguous.
            if let Some(result) =
                declaration_only_package_result(&context.active_document_path, request)
            {
                return Ok(result);
            }

            // A native-binary-only package (a `bin` plus a platform-specific native binary as
            // `optionalDependencies`, no importable JS entry) is likewise MEASURED at zero and
            // labelled, rather than shown as a bare "unavailable" (B3).
            if let Some(result) =
                native_binary_only_package_result(&context.active_document_path, request)
            {
                return Ok(result);
            }

            return Err(error);
        }
        Err(error) => return Err(error),
    };
    // The non-resolved path has no caller that needs the analyzed graph, so it
    // discards it and keeps returning a bare `ImportResult`.
    let (result, _graph) = analyze_import_inner_resolved(context, request, resolved)?;
    Ok(result)
}

fn resolve_import_package(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ResolvedPackage, AnalysisError> {
    resolve_package_entry(&context.active_document_path, request).map_err(|message| {
        let stage = if message.contains("unsafe package name") {
            crate::pipeline::stage::PACKAGE_VALIDATION
        } else if message.contains("package manifest not found") {
            crate::pipeline::stage::PACKAGE_RESOLUTION
        } else if is_manifest_fallback_error(&message) {
            crate::pipeline::stage::PACKAGE_MANIFEST
        } else {
            crate::pipeline::stage::ENTRY_RESOLUTION
        };
        let details = resolver_details(&message);
        error_with_context(stage, message, context, request, details)
    })
}

fn is_manifest_fallback_error(message: &str) -> bool {
    message.contains("failed to read package manifest")
        || message.contains("failed to parse package manifest")
        || message.contains("missing a string version")
}

fn analyze_import_inner_resolved(
    context: &AnalysisContext,
    request: &ImportRequest,
    resolved: ResolvedPackage,
) -> Result<(ImportResult, Option<FingerprintSource>), AnalysisError> {
    let side_effects_mode = resolved.side_effects;
    let entry_path = resolved.entry_path;
    let package_root = resolved.package_root;
    let is_cjs = resolved.is_cjs;
    let package_json = resolved.package_json;

    // A stat failure is an IO condition, not a fact about the package (a lock, a permission blip, a
    // drive that blinked), so `entry_metadata` is NOT durable — see `pipeline::stage`. It used to be
    // cached, and expired only when the package's manifest changed.
    let metadata = fs::metadata(&entry_path).map_err(|error| {
        error_with_context(
            crate::pipeline::stage::ENTRY_METADATA,
            format!(
                "failed to stat package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;

    // An entry over the module source limit used to be sized from the entry file ALONE — the
    // whole graph behind it uncounted — and that number was served as the import's size. It is a
    // deterministic property of the package's bytes that the engine cannot answer, so it is
    // Unmeasured: `oversized_entry` is not in `stage::ALL`, hence not transient, hence cached
    // like any other fact about the code.
    if metadata.len() as usize > MAX_MODULE_SOURCE_BYTES {
        return Err(error_with_context(
            crate::pipeline::stage::OVERSIZED_ENTRY,
            format!(
                "entry file exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit, so no module graph could be built"
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        ));
    }

    // A failed engine build is Unmeasured. It used to degrade to that same entry-file-alone
    // sizing, which carried `error: None` plus a plausible byte count — the fabricated state
    // every `!result.error` check in the system waves through.
    let (mut result, loaded_paths, freshness) = analyze_with_rolldown_engine(
        context,
        request,
        &entry_path,
        &package_root,
        &side_effects_mode,
        is_cjs,
    )?;
    // A package whose JS entry resolved but which is backed by a platform-specific native binary
    // keeps its measured JS size and carries a `native_binary` flag beside it, so a thin shim (the
    // TypeScript 7 version stub) is not read as the whole cost (B3).
    annotate_native_binary(&mut result, &package_json);
    record_loaded_paths(entry_path, request.runtime, loaded_paths);
    Ok((result, Some(freshness)))
}

/// Rolldown-backed analysis (spec §8): one engine build produces the raw
/// chunk, OXC minifies it, and the existing compression pipeline runs over
/// the minified string. Returns the loaded real paths (plus the package
/// manifest) for §8.3 freshness fingerprints alongside the result.
///
pub(crate) fn analyze_with_rolldown_engine(
    context: &AnalysisContext,
    request: &ImportRequest,
    entry_path: &Path,
    package_root: &Path,
    side_effects_mode: &SideEffectsMode,
    is_cjs: bool,
) -> Result<(ImportResult, Vec<PathBuf>, FingerprintSource), AnalysisError> {
    use crate::engine::{BundleEntry, BundlePurpose, BundleRequest, BundleSelection, boundary};

    let bundle_entry = |selection: BundleSelection| BundleEntry {
        entry_path: entry_path.to_path_buf(),
        package_root: package_root.to_path_buf(),
        selection,
    };
    let artifact = boundary::bundle_sync(BundleRequest {
        entries: vec![bundle_entry(engine_selection(request))],
        runtime: request.runtime,
        purpose: BundlePurpose::ImportSize,
    })
    .map_err(|failure| engine_error(context, request, failure))?;

    let minified = minify_source(&artifact.code, false).map_err(|error| {
        error_with_context(
            crate::pipeline::stage::MINIFY,
            format!("failed to minify engine chunk: {error}"),
            context,
            request,
            vec![source_excerpt_detail(&artifact.code)],
        )
    })?;
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            crate::pipeline::stage::COMPRESSION,
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;

    // The package's non-JavaScript assets, processed the way they really ship, so their bytes JOIN
    // the Import Cost instead of being disclosed beside a number that excluded them (B2). Each
    // artifact is compressed on its own and summed (ADR-0005). This never fails: an asset it cannot
    // process falls back to the raw-byte disclosure that was the whole behaviour before B2.
    let assets = process_assets(&artifact.assets);
    let asset_sizes = assets.total();

    // §7.4/FR-021: Side-Effectful is a property of THE IMPORT — is the entry being measured one
    // the package declares effectful? — so the glob form answers by MATCHING the entry, and
    // `has_side_effects` is the whole answer.
    //
    // It used to be ORed with `is_array()`, which overrode that correct answer with an
    // unconditional `true` for every array declaration. `"sideEffects": ["**/*.css"]` says nothing
    // about a JavaScript entry, and it is an everyday declaration — so an everyday package was
    // reported side-effectful, forced `truly_treeshakeable: false` BY CONSTRUCTION (the comparison
    // below is gated on `!side_effects` and never ran), and could never reach High confidence. The
    // premise that bought that conservatism — "glob matching unavailable from public bundler
    // metadata" — was retracted by the §10.7 amendment, and the matcher is now Rolldown's own.
    let side_effects = side_effects_mode.has_side_effects();
    let mut diagnostics: Vec<ImportDiagnostic> = artifact
        .diagnostics
        .iter()
        .map(|diagnostic| ImportDiagnostic {
            stage: diagnostic.stage.clone(),
            message: diagnostic.message.clone(),
            details: Vec::new(),
        })
        .collect();
    // An asset that could not be processed keeps the old disclosure: its bytes are real, they ship,
    // and they are NOT in the number — which is exactly what this stage has always meant.
    diagnostics.extend(asset_diagnostics(&assets));

    // Full-package comparison (§8.4/§6.3): a second engine build measures the
    // complete surface; failure degrades to "not treeshakeable", never an
    // analysis error.
    //
    // The answer does not depend on *which* names were imported, but the import
    // cache key does — so without the memo, N named variants of one entry cost N
    // of these builds on top of their own. `full_package::lookup` re-checks the
    // fingerprints of the exact bytes the stored length was measured from, so it
    // expires precisely when the length it holds would have gone wrong.
    let mut truly_treeshakeable = false;
    if !side_effects
        && matches!(request.import_kind, ImportKind::Named)
        && !request.named.is_empty()
    {
        let full_len = full_package::lookup(entry_path, request.runtime).or_else(|| {
            // Read before the build, not after: an invalidation landing while this build
            // is in flight must not be stamped onto a length measured from the bytes it
            // invalidated.
            let generation = crate::cache::memory::cache_generation();
            let full = match boundary::bundle_sync(BundleRequest {
                entries: vec![bundle_entry(BundleSelection::Full)],
                runtime: request.runtime,
                purpose: BundlePurpose::FullPackageComparison,
            }) {
                Ok(full) => full,
                Err(failure) => {
                    // Reported under the stage the comparison build actually failed at, not
                    // under a label invented here (§12, same rule as `engine_fallback_diagnostic`
                    // — the fallback is expressed by the message, not by erasing where it broke).
                    // That is also what lets `should_cache_result` see a TRANSIENT failure here:
                    // `truly_treeshakeable: false` is a fabricated fact when the build that would
                    // have disproved it merely timed out, and caching it would mark a healthy
                    // package "not tree-shakeable" for a whole cache generation.
                    diagnostics.push(ImportDiagnostic {
                        stage: contract_stage(&failure.stage).to_owned(),
                        message: format!(
                            "full-package comparison build failed; treating as not tree-shakeable: {}",
                            failure.message
                        ),
                        details: Vec::new(),
                    });
                    return None;
                }
            };

            let full_len = minify_source(&full.code, false).ok()?.len() as u64;
            // A graph carrying a module the plugin could not fingerprint as it read it
            // (a binary module) has no complete read-time record, so there is nothing to
            // expire a memo against: measure it, use it, and store nothing.
            if full.unhashed_paths.is_empty() {
                full_package::store(
                    entry_path,
                    request.runtime,
                    full_len,
                    full_package_fingerprints(context, package_root, &full),
                    generation,
                );
            }
            Some(full_len)
        });

        if let Some(full_len) = full_len
            && full_len > 0
        {
            // Mirror the legacy predicate: within 5% of the full size is not
            // truly tree-shakeable.
            let ratio = (minified.len() as f64) / (full_len as f64);
            truly_treeshakeable = ratio <= 0.95;
        }
    }

    let (confidence, confidence_reasons) = engine_confidence(side_effects, &diagnostics);
    let contributions: Vec<ModuleContribution> = artifact
        .contributions
        .iter()
        .map(|contribution| ModuleContribution {
            path: contribution.path.to_string_lossy().to_string(),
            bytes: contribution.rendered_bytes as u64,
        })
        .collect();

    // §8.3: freshness comes from fingerprints captured by the same reads that supplied every
    // measured byte. The plugin owns JavaScript and directly imported asset snapshots; the asset
    // processor owns CSS `@import` children and local resources discovered through `url()`.
    let mut stat_paths = artifact.unhashed_paths;
    stat_paths.push(package_root.join("package.json"));
    stat_paths.extend(first_party_manifests(context, &artifact.loaded_paths));

    let mut fingerprints = artifact.read_time_fingerprints.clone();
    fingerprints.extend(assets.freshness_fingerprints());
    crate::cache::key::sort_and_dedup_fingerprints(&mut fingerprints);
    let freshness = FingerprintSource::ReadTime {
        fingerprints,
        stat_paths,
    };
    let mut loaded_paths = artifact.loaded_paths;
    loaded_paths.extend(assets.read_paths.iter().cloned());
    loaded_paths.sort();
    loaded_paths.dedup();

    let mut result = ImportResult::measured(
        request.specifier.clone(),
        MeasuredSizes {
            raw_bytes: artifact.code.len() as u64 + asset_sizes.raw_bytes,
            minified_bytes: minified.len() as u64 + asset_sizes.minified_bytes,
            gzip_bytes: compressed.gzip_bytes + asset_sizes.gzip_bytes,
            brotli_bytes: compressed.brotli_bytes + asset_sizes.brotli_bytes,
            zstd_bytes: compressed.zstd_bytes + asset_sizes.zstd_bytes,
        },
    );
    result.side_effects = side_effects;
    result.truly_treeshakeable = truly_treeshakeable;
    result.is_cjs = is_cjs;
    result.confidence = confidence;
    result.confidence_reasons = confidence_reasons;
    result.diagnostics = diagnostics;
    result.module_breakdown = Some(top_module_contributions(&contributions));
    // How the number above is composed: these bytes are already IN the five sizes, and this says
    // which of them are stylesheet, wasm, or font, so a UI kit's cost is legible rather than a
    // single opaque figure (B2).
    result.asset_breakdown = assets.contributions;
    result.internal_contributions = contributions;

    Ok((result, loaded_paths, freshness))
}

pub(crate) fn engine_selection(request: &ImportRequest) -> crate::engine::BundleSelection {
    use crate::engine::BundleSelection;
    match request.import_kind {
        ImportKind::Named if !request.named.is_empty() => {
            BundleSelection::Named(request.named.clone())
        }
        // No requested names known: measure the full surface conservatively,
        // matching the legacy empty-bundle fallback.
        ImportKind::Named => BundleSelection::Full,
        ImportKind::Default => BundleSelection::Default,
        ImportKind::Namespace => BundleSelection::Namespace,
        ImportKind::Dynamic => BundleSelection::Full,
    }
}

/// The contract's failure stages are a closed vocabulary; keep the known ones verbatim so
/// cache and diagnostic consumers see stable stage names, and collapse anything unknown to
/// `generate` rather than inventing a label.
///
/// The vocabulary is *derived* from `engine::stage::ALL` rather than restated here. It used
/// to be restated, and the restatement drifted: the boundary's `panic`, `timeout` and
/// `engine_gone` were missing, so a daemon-side panic reached the user relabelled as an
/// ordinary codegen failure — indistinguishable from one — while `file_size.rs` passed the
/// same stage through untouched, giving one failure two names in two different responses.
/// Deriving the list makes that class of drift impossible instead of merely testable.
fn contract_stage(stage: &str) -> &'static str {
    crate::engine::stage::ALL
        .iter()
        .copied()
        .find(|known| *known == stage)
        .unwrap_or(crate::engine::stage::GENERATE)
}

fn engine_error(
    context: &AnalysisContext,
    request: &ImportRequest,
    failure: crate::engine::BundleFailure,
) -> AnalysisError {
    let mut error = error_with_context(
        contract_stage(&failure.stage),
        failure.message,
        context,
        request,
        failure
            .diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.stage, diagnostic.message))
            .collect(),
    );
    // The bytes the build read before it gave up. A cached deterministic failure expires against
    // exactly these (see `AnalysisError::read_time_fingerprints`).
    error.read_time_fingerprints = failure.read_time_fingerprints;
    error.loaded_paths = failure.loaded_paths;
    error
}

fn top_module_contributions(contributions: &[ModuleContribution]) -> Vec<ModuleContribution> {
    let mut contributions = contributions.to_vec();
    contributions.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    contributions.truncate(10);
    contributions
}

fn engine_confidence(
    side_effects: bool,
    diagnostics: &[ImportDiagnostic],
) -> (ConfidenceLevel, Vec<String>) {
    if !side_effects && diagnostics.is_empty() {
        return (
            ConfidenceLevel::High,
            vec![
                "Rolldown linking plus OXC validation, minification, and compression completed without precision warnings."
                    .to_owned(),
            ],
        );
    }

    let mut reasons = Vec::new();
    if side_effects {
        // The engine builds the same named-selection entry whether or not the package
        // declares side effects; what changes is that an effectful package cannot be
        // certified as fully tree-shakeable. The old text described the deleted
        // engine, which really did switch to full-graph sizing here.
        reasons.push(
            "Package declares side effects, so modules it retains cannot be certified as \
             tree-shaken away."
                .to_owned(),
        );
    }

    if !diagnostics.is_empty() {
        let mut stages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.stage.as_str())
            .collect::<Vec<_>>();
        stages.sort_unstable();
        stages.dedup();
        reasons.push(format!(
            "Analysis emitted diagnostics that can reduce precision: {}.",
            stages.join(", ")
        ));
    }

    if reasons.is_empty() {
        reasons.push("Engine pipeline completed with conservative assumptions.".to_owned());
    }

    (ConfidenceLevel::Medium, reasons)
}

/// The Unmeasured result an analysis failure becomes.
///
/// It used to carry five **zero** sizes. That is the same lie as a fabricated one, told with a
/// smaller number: `0 B` reads as "this import is free", and every consumer that summed or
/// compared it did so. There is no size now, and the stage says why there is not.
fn error_result(request: &ImportRequest, error: AnalysisError) -> ImportResult {
    ImportResult::unmeasured(
        request.specifier.clone(),
        error.stage,
        error.message,
        error.details,
    )
}

fn resolver_details(message: &str) -> Vec<String> {
    message
        .split("; ")
        .filter(|part| part.starts_with("checked:") || part.starts_with("candidate:"))
        .map(str::to_owned)
        .collect()
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
        read_time_fingerprints: Vec::new(),
        loaded_paths: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Editing a first-party workspace dependency's manifest changes what the bundler
    /// resolves and retains, while none of its source files move. Without the manifest
    /// in the fingerprint set the cached size is served as fresh (spec R5).
    ///
    /// Loaded paths are canonicalized, so a workspace package linked into
    /// `node_modules` (as pnpm does) resolves to its real path and is correctly seen
    /// as first-party.
    #[test]
    fn first_party_manifests_are_fingerprint_inputs_and_installed_ones_are_not() {
        let workspace = std::env::temp_dir().join(format!(
            "import-lens-r5-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let ui = workspace.join("packages").join("ui");
        std::fs::create_dir_all(ui.join("src")).expect("ui src");
        std::fs::write(ui.join("package.json"), r#"{"name":"ui"}"#).expect("ui manifest");
        std::fs::write(ui.join("src").join("index.ts"), "export const a = 1;\n")
            .expect("ui source");

        let installed = workspace.join("node_modules").join("left-pad");
        std::fs::create_dir_all(&installed).expect("installed dir");
        std::fs::write(installed.join("package.json"), r#"{"name":"left-pad"}"#)
            .expect("installed manifest");
        std::fs::write(installed.join("index.js"), "export const b = 2;\n")
            .expect("installed source");

        let context = AnalysisContext {
            workspace_root: workspace.clone(),
            active_document_path: workspace.join("src").join("app.ts"),
        };

        let manifests = first_party_manifests(
            &context,
            &[
                ui.join("src").join("index.ts"),
                installed.join("index.js"),
                // A second module in the same package must not duplicate the manifest.
                ui.join("src").join("other.ts"),
            ],
        );

        assert_eq!(
            manifests,
            vec![ui.join("package.json")],
            "the first-party dependency's manifest is a freshness input; an installed \
             package's is covered by the install generation and must not be"
        );

        std::fs::remove_dir_all(&workspace).ok();
    }

    /// The other half of `contract_stage`: a stage the vocabulary does not know collapses to
    /// `generate` rather than reaching the client under an invented label.
    ///
    /// There is deliberately no companion test asserting that every stage in `stage::ALL`
    /// survives the edge. `contract_stage` *searches* `ALL`, so such a test is identity over
    /// `ALL` by construction and can never go red — it would only look like coverage. The
    /// property it pretended to protect (a declared stage is in `ALL`) is now structural:
    /// `engine::stage` emits the constants and `ALL` from one macro invocation, so a stage
    /// that is missing from `ALL` cannot be written.
    #[test]
    fn an_unknown_stage_collapses_to_generate() {
        assert_eq!(
            contract_stage("something-nobody-defined"),
            crate::engine::stage::GENERATE,
            "an unrecognized stage collapses rather than inventing a label"
        );
    }
}
