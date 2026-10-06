use crate::engine::scheduling::{drain_classified, drain_misses_owned, drain_ordered_owned};
use crate::{
    analysis_flight::AnalysisFlightRegistry,
    cache::{
        key::{FileFingerprint, cache_key_for_resolved_import},
        memory::ImportCache,
        project::ProjectCacheRegistry,
    },
    document::{
        IgnoreRuleResolver, analyze_imports, get_package_name, is_runtime_package_specifier,
        named_import_completion_context, package_json_dependency_entries,
        package_json_dependency_sections, runtime_at_offset, should_ignore_import,
    },
    ipc::protocol::{
        AnalyzeDocumentRequest, AnalyzeDocumentResponse, AnalyzePackageJsonRequest,
        AnalyzePackageJsonResponse, AnalyzeSpecifiersRequest, AnalyzeSpecifiersResponse,
        CacheListRequest, CacheListResponse, CacheRemoveRequest, CacheRemoveResponse,
        CacheRemoveScope, CacheStatusRequest, CacheStatusResponse, CompleteImportMembersRequest,
        CompleteImportMembersResponse, DetectedImport, EnumerateExportsRequest,
        EnumerateExportsResponse, FileSizeDocumentRequest, FileSizeDocumentResponse, FreshnessKind,
        ImportAnalysisItem, ImportAnalysisStatus, ImportDiagnostic, ImportKind, ImportRequest,
        ImportResult, ImportRuntime, ImportSyntax, PROTOCOL_VERSION,
        PackageJsonDependencyAnalysisItem, RefreshedImportIdentity,
        RegistryHintMode as ProtocolRegistryHintMode, RegistryHintResult, RegistryHintTarget,
        WorkspaceReportRequest, WorkspaceReportResponse, WorkspaceReportSummary,
        is_supported_protocol_version,
    },
    pipeline::analyze::{
        AnalysisContext, analyze_resolved_import_with_dependencies, analyze_unresolved_import,
    },
    pipeline::file_size::{SizedImport, annotate_shared_bytes, compute_file_size},
    pipeline::resolver::{
        FirstPartySourceProbe, ResolvedPackage, find_package_root, resolve_package_entry,
    },
};
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Whether a cached-analyze read promotes the entry's LRU recency (FR-026b, §5.1).
/// The workspace report reads `Bulk` so a full-workspace pass cannot evict the
/// user's warm working set. When intent is ambiguous, prefer `Interactive`:
/// over-promoting is safe.
#[derive(Clone, Copy)]
enum ReadIntent {
    Interactive,
    Bulk,
}

/// The outcome of a cache lookup, before any engine build.
///
/// A batch classifies every import at pool width and hands only the misses to the
/// engine drain. `Miss` carries the resolved package and key so the build half does
/// not re-read the manifest.
enum CacheProbe {
    Hit(Box<ImportResult>),
    /// Boxed: this rides in the `Err` arm of the classify closure for every import in
    /// a batch, and `ResolvedPackage` dwarfs the discriminant.
    Miss(Box<PendingBuild>),
    /// The package entry did not resolve. The answer is built from this message, never from a
    /// second resolution that could start a build.
    Unresolved(String),
}

struct PendingBuild {
    resolved: ResolvedPackage,
    key: String,
}

/// One import a streamed response answered `Loading`: everything needed to build it after the
/// response has gone out and to address the result back to the right import (specifier alone is
/// not unique: two imports of one package differ by kind and named exports).
pub struct PendingImport {
    detected: DetectedImport,
    request: ImportRequest,
    pending: Box<PendingBuild>,
}

/// One import a response already carried a real measurement for, addressed by the same identity a
/// push uses.
///
/// The streamed builds need these: shared-module bytes are a property of the WHOLE document, so
/// the closing annotation pass cannot see only the imports that arrived late.
///
/// The runtime rides on `identity` (two runtime variants of one Astro import are two rows).
/// `ImportRuntime` also lives on `ImportRequest`: sharing partitions on `DetectedImport.runtime`,
/// the build on `ImportRequest.runtime`. They agree only because [`import_request_for_detected`]
/// is the single derivation copying one into the other; a mixed-runtime file under-reports if it
/// diverges (pinned by `tests/file_size_runtime.rs`).
#[derive(Clone)]
pub struct MeasuredImport {
    pub result: ImportResult,
    pub identity: RefreshedImportIdentity,
}

/// A document analysis that did not wait for the engine: what the cache answered now, what is
/// still to be built, and the measurements already sent (for the closing shared-bytes pass).
pub struct StreamedDocumentAnalysis {
    pub response: AnalyzeDocumentResponse,
    pub measured: Vec<MeasuredImport>,
    pub pending: Vec<PendingImport>,
}

impl StreamedDocumentAnalysis {
    /// A response with nothing left to build (a protocol/parse error, or a document whose every
    /// import the cache answered).
    pub fn settled(response: AnalyzeDocumentResponse) -> Self {
        Self {
            response,
            measured: Vec::new(),
            pending: Vec::new(),
        }
    }
}

/// The cache-only classification of a document's imports, before any build runs.
struct CachedDocumentAnalysis {
    items: Vec<ImportAnalysisItem>,
    pending: Vec<PendingImport>,
}

impl CachedDocumentAnalysis {
    /// The imports this classification could already measure, in the shape the streamed builds
    /// need for their closing shared-bytes pass.
    fn measured(&self) -> Vec<MeasuredImport> {
        self.items
            .iter()
            .filter_map(|item| {
                Some(MeasuredImport {
                    result: item.result.clone()?,
                    identity: RefreshedImportIdentity {
                        specifier: item.detected.specifier.clone(),
                        import_kind: item.detected.import_kind,
                        named: item.detected.named.clone(),
                        runtime: item.detected.runtime,
                    },
                })
            })
            .collect()
    }
}

const SLOW_CACHE_LOOKUP_LOG_THRESHOLD: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct ComputedAnalysis {
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
    dependencies_are_reusable: bool,
}

/// Trailing re-check after a background SWR revalidation re-inserts a key. `Stale`
/// means a dependency changed again during the recompute (and a concurrent stale
/// serve was coalesced away), so exactly one more revalidation runs. A graduated
/// transient `Unknown` is never re-armed: it would re-hit the same stat/read error
/// and could overwrite the good cached value.
fn should_rearm_revalidation(freshness: Option<crate::cache::key::Freshness>) -> bool {
    matches!(freshness, Some(crate::cache::key::Freshness::Stale))
}

/// Process-global "a cache-maintenance pass is running" flag. The on-disk cache is
/// shared by every service instance, and a re-Hello schedules a fresh pass that
/// can overlap the previous connection's detached `spawn_blocking` pass. redb's
/// single writer already serializes them; this skips the duplicate scan.
static CACHE_MAINTENANCE_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// RAII claim on [`CACHE_MAINTENANCE_IN_PROGRESS`]. Clears the flag on drop,
/// including on unwind, so a panicking pass cannot wedge maintenance off.
struct MaintenanceGuard;

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        CACHE_MAINTENANCE_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Claims the maintenance flag: `Some(guard)` for the winner, which runs the pass;
/// `None` while a pass is already in flight.
fn try_begin_cache_maintenance() -> Option<MaintenanceGuard> {
    CACHE_MAINTENANCE_IN_PROGRESS
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_ok()
        .then_some(MaintenanceGuard)
}

fn registry_hint_service_mode(
    mode: ProtocolRegistryHintMode,
) -> crate::registry::service::RegistryHintMode {
    match mode {
        ProtocolRegistryHintMode::RefreshStale => {
            crate::registry::service::RegistryHintMode::RefreshStale
        }
        ProtocolRegistryHintMode::ForceRefresh => {
            crate::registry::service::RegistryHintMode::ForceRefresh
        }
        ProtocolRegistryHintMode::Off | ProtocolRegistryHintMode::Cached => {
            crate::registry::service::RegistryHintMode::Cached
        }
    }
}

fn registry_hint_result_from_lookup(
    target: RegistryHintTarget,
    lookup: crate::registry::types::RegistryHintLookup,
) -> RegistryHintResult {
    let origin = match lookup.origin {
        crate::registry::types::RegistryHintOrigin::Cache => "cache",
        crate::registry::types::RegistryHintOrigin::Network => "network",
    };
    RegistryHintResult {
        target,
        hint: lookup.hint,
        error: lookup.error,
        origin: Some(origin.to_owned()),
    }
}

// Not `Debug`: `RegistryHintService` holds trait objects and `RegistryRefreshExecutor` a pool.
pub struct ImportLensService {
    cache_registry: ProjectCacheRegistry,
    analysis_flights: AnalysisFlightRegistry<ComputedAnalysis>,
    registry_hints: crate::registry::service::RegistryHintService,
    registry_executor: crate::registry::executor::RegistryRefreshExecutor,
    report_executor: crate::report::executor::WorkspaceReportExecutor,
    // Registry-metadata store byte budget (`importLens.registryCacheMaxSizeMB`, from Hello);
    // the maintenance pass caps the registry store at this.
    registry_cache_max_size_bytes: u64,
    // Set only by `new_with_registry_hints_for_tests`: the IPC server's Hello handler then keeps
    // `registry_hints`/`registry_executor` across the rebuild, so an injected fake
    // `RegistryHttpClient` survives the handshake. See `ipc/server.rs`'s `Hello` handling.
    preserve_registry_across_hello: bool,
}

impl ImportLensService {
    pub fn new(storage_path: Option<PathBuf>, enable_disk_cache: bool) -> Self {
        Self::new_with_cache_policy(
            storage_path,
            enable_disk_cache,
            512,
            crate::registry::constants::REGISTRY_CACHE_MAX_SIZE_BYTES / (1024 * 1024),
        )
    }

    pub fn new_with_cache_policy(
        storage_path: Option<PathBuf>,
        enable_disk_cache: bool,
        cache_max_size_mb: u64,
        registry_cache_max_size_mb: u64,
    ) -> Self {
        let cache_registry =
            ProjectCacheRegistry::new(storage_path.clone(), enable_disk_cache, cache_max_size_mb);
        let registry_hints = storage_path
            .clone()
            .map(|path| {
                crate::registry::service::RegistryHintService::new(
                    crate::registry::cache::RegistryMetadataCache::new(path),
                    Box::new(crate::registry::client::UreqRegistryHttpClient::default()),
                )
            })
            .unwrap_or_else(crate::registry::service::RegistryHintService::disabled);
        let registry_executor = crate::registry::executor::RegistryRefreshExecutor::new(
            crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
        );
        let report_executor = crate::report::executor::WorkspaceReportExecutor::new();
        Self {
            cache_registry,
            analysis_flights: AnalysisFlightRegistry::new(),
            registry_hints,
            registry_executor,
            report_executor,
            registry_cache_max_size_bytes: registry_cache_max_size_mb.saturating_mul(1024 * 1024),
            preserve_registry_across_hello: false,
        }
    }

    /// Test-only: lets integration tests (an external crate, so `#[cfg(test)]` is invisible to
    /// them) inject a fake `RegistryHintService`. See `preserve_registry_across_hello`.
    pub fn new_with_registry_hints_for_tests(
        registry_hints: crate::registry::service::RegistryHintService,
    ) -> Self {
        Self {
            cache_registry: ProjectCacheRegistry::new(None, false, 512),
            analysis_flights: AnalysisFlightRegistry::new(),
            registry_hints,
            registry_executor: crate::registry::executor::RegistryRefreshExecutor::new(
                crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
            ),
            report_executor: crate::report::executor::WorkspaceReportExecutor::new(),
            registry_cache_max_size_bytes:
                crate::registry::constants::REGISTRY_CACHE_MAX_SIZE_BYTES,
            preserve_registry_across_hello: true,
        }
    }

    /// Test-only: seeds cached registry metadata without a network fetch. Not
    /// `#[cfg(test)]`-gated, for the reason on `new_with_registry_hints_for_tests`.
    pub fn registry_hints_for_tests(&self) -> RegistryHintTestHandle<'_> {
        RegistryHintTestHandle { service: self }
    }

    /// Rebuilds only the cache registry for a new Hello, keeping `registry_hints` and
    /// `registry_executor`. Called only when `preserve_registry_across_hello()` is true;
    /// production rebuilds via `new_with_cache_policy` so `hello.storage_path` stays the
    /// source of truth for registry configuration.
    pub fn rebuild_cache_registry_for_hello(
        self,
        storage_path: Option<PathBuf>,
        enable_disk_cache: bool,
        cache_max_size_mb: u64,
        registry_cache_max_size_mb: u64,
    ) -> Self {
        Self {
            cache_registry: ProjectCacheRegistry::new(
                storage_path,
                enable_disk_cache,
                cache_max_size_mb,
            ),
            analysis_flights: self.analysis_flights,
            registry_hints: self.registry_hints,
            registry_executor: self.registry_executor,
            report_executor: self.report_executor,
            registry_cache_max_size_bytes: registry_cache_max_size_mb.saturating_mul(1024 * 1024),
            preserve_registry_across_hello: self.preserve_registry_across_hello,
        }
    }

    pub fn preserve_registry_across_hello(&self) -> bool {
        self.preserve_registry_across_hello
    }

    /// Startup recency seed. The Hello handler calls this synchronously, after the registry is
    /// rebuilt with the negotiated disk config and before any analyze/cache request is served,
    /// so no new entry gets a pre-seed low seq. See
    /// `ProjectCacheRegistry::seed_recency_clock_from_disk`.
    pub fn seed_recency_clock_from_disk(&self) {
        self.cache_registry.seed_recency_clock_from_disk();
    }

    pub fn refresh_registry_hint_target(
        &self,
        target: RegistryHintTarget,
        mode: ProtocolRegistryHintMode,
        now_ms: u64,
    ) -> RegistryHintResult {
        let lookup = self.registry_hints.hint_for(
            &target.name,
            target.installed_version.as_deref(),
            registry_hint_service_mode(mode),
            now_ms,
        );

        registry_hint_result_from_lookup(target, lookup)
    }

    pub fn spawn_registry_refresh(&self, job: impl FnOnce() + Send + 'static) {
        self.registry_executor.spawn(job);
    }

    /// Fans a bulk "refresh dependency block" onto the isolated registry pool.
    ///
    /// * **Cache first.** One cache-only pre-pass streams cache-eligible results
    ///   immediately; only the rest are enqueued for network refresh.
    /// * **Bounded in flight.** Each target is a `spawn` onto the
    ///   `REGISTRY_REFRESH_CONCURRENCY`-thread pool; the pool is the in-flight cap.
    /// * **Cancellable.** Each job re-reads `cancelled` before its fetch. Once a
    ///   newer block supersedes this one (or the connection ends), queued jobs
    ///   report `None` without an error; jobs in flight finish.
    ///
    /// `on_result` runs exactly once per target with its index and the result, or
    /// `None` when skipped by cancellation. Workers still honor single-flight, the
    /// registry cooldowns, and the shared rate limiter.
    pub fn spawn_registry_refresh_block<F>(
        self: &std::sync::Arc<Self>,
        targets: Vec<RegistryHintTarget>,
        mode: ProtocolRegistryHintMode,
        now_ms: u64,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        on_result: F,
    ) where
        F: Fn(usize, Option<RegistryHintResult>) + Send + Clone + 'static,
    {
        let mut pending = Vec::with_capacity(targets.len());
        for (index, target) in targets.into_iter().enumerate() {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                let on_result = on_result.clone();
                on_result(index, None);
                continue;
            }
            pending.push((index, target));
        }

        let cached_lookups = {
            let lookup_targets: Vec<_> = pending
                .iter()
                .map(|(_, target)| (target.name.as_str(), target.installed_version.as_deref()))
                .collect();
            self.registry_hints.cached_lookups_for_mode(
                &lookup_targets,
                registry_hint_service_mode(mode),
                now_ms,
            )
        };

        for ((index, target), cached_lookup) in pending.into_iter().zip(cached_lookups) {
            let on_result = on_result.clone();
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                on_result(index, None);
                continue;
            }
            if let Some(lookup) = cached_lookup {
                on_result(
                    index,
                    Some(registry_hint_result_from_lookup(target, lookup)),
                );
                continue;
            }

            let svc = std::sync::Arc::clone(self);
            let cancelled = std::sync::Arc::clone(&cancelled);
            self.spawn_registry_refresh(move || {
                // Acquire pairs with the Release store on the supersede/disconnect side.
                let outcome = if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    None
                } else {
                    let target_for_error = target.clone();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        svc.refresh_registry_hint_target(target, mode, now_ms)
                    }))
                    .unwrap_or_else(|_| {
                        crate::logging::log_warn(
                            "registry",
                            format!("registry worker panicked for {}", target_for_error.name),
                        );
                        RegistryHintResult {
                            target: target_for_error,
                            hint: None,
                            error: Some("registry worker panicked".to_owned()),
                            origin: None,
                        }
                    });
                    Some(result)
                };
                on_result(index, outcome);
            });
        }
    }

    pub fn flush_registry_hints(&self) {
        self.registry_hints.flush();
    }

    fn build_workspace_report_on_worker(
        &self,
        request: WorkspaceReportRequest,
    ) -> WorkspaceReportResponse {
        if !is_supported_protocol_version(request.version) {
            return WorkspaceReportResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                rows: Vec::new(),
                summary: WorkspaceReportSummary::default(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        // The aggregation runs on a fire-and-forget worker: an uncaught panic would drop the
        // `oneshot` sender and surface only a generic transport error, so it becomes an explicit
        // error response. `AssertUnwindSafe` is sound: a poisoned cache mutex is handled by the
        // cache's poisoned-lock fallback, and no `&mut` state straddles the boundary.
        let version = request.version;
        let request_id = request.request_id;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.build_workspace_report_inner(request)
        }))
        .unwrap_or_else(|_| {
            crate::logging::log_warn(
                "report",
                "workspace report aggregation panicked; returning error response",
            );
            WorkspaceReportResponse {
                version,
                request_id,
                rows: Vec::new(),
                summary: WorkspaceReportSummary::default(),
                error: Some("workspace report aggregation panicked".to_owned()),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "workspace_report",
                    "aggregation panicked",
                )],
            }
        })
    }

    fn build_workspace_report_inner(
        &self,
        request: WorkspaceReportRequest,
    ) -> WorkspaceReportResponse {
        // Forces an aggregation panic (outside per-file analysis) to exercise the `catch_unwind`
        // in `build_workspace_report_on_worker`.
        #[cfg(test)]
        {
            if request
                .workspace_root
                .contains("__IMPORTLENS_FORCE_REPORT_PANIC__")
            {
                panic!("forced workspace report aggregation panic (test only)");
            }
        }

        let workspace_root = PathBuf::from(&request.workspace_root);
        let files = crate::report::scanner::scan_workspace_sources(&workspace_root);
        // One resolver per report run: files in one directory share a .importlensignore walk, and
        // edits between reports are re-read.
        let ignore_resolver = IgnoreRuleResolver::default();
        // Reading and import detection need no engine permit, so this runs at the width of the
        // report's dedicated pool; only the misses go through the engine drain.
        let items = files
            .par_iter()
            .flat_map_iter(|source_path| {
                let source = match fs::read_to_string(source_path) {
                    Ok(source) => source,
                    Err(_) => return Vec::new().into_iter(),
                };
                self.analyze_report_source(source_path, &request, source, &ignore_resolver)
                    .into_iter()
            })
            .collect::<Vec<_>>();
        let row_set = crate::report::model::build_report_rows(&items, &request.budgets);
        let summary = crate::report::model::build_report_summary(&row_set);

        WorkspaceReportResponse {
            version: request.version,
            request_id: request.request_id,
            rows: row_set.rows,
            summary,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    fn analyze_report_source(
        &self,
        source_path: &std::path::Path,
        request: &WorkspaceReportRequest,
        source: String,
        ignore_resolver: &IgnoreRuleResolver,
    ) -> Vec<crate::report::model::WorkspaceReportItem> {
        let document_request = AnalyzeDocumentRequest {
            message_type: "analyze_document".to_owned(),
            version: request.version,
            request_id: request.request_id,
            workspace_root: request.workspace_root.clone(),
            active_document_path: source_path.to_string_lossy().to_string(),
            source,
        };

        // A panic in one file degrades to a skipped file, not a failed report. AssertUnwindSafe
        // is sound: a poisoned cache mutex is handled by the cache's poisoned-lock fallback.
        let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            {
                if document_request
                    .source
                    .contains("__IMPORTLENS_FORCE_PANIC__")
                {
                    panic!("forced report analysis panic (test only)");
                }
            }

            // `Bulk`: a full-workspace scan must not evict the user's warm set (§5.1).
            //
            // The report waits for every build (a "still measuring" row is not a row), so a
            // workspace with enough parked packages can outlive the client's 300s, as the SRS
            // states.
            self.handle_analyze_document_with_intent(
                document_request,
                ignore_resolver,
                ReadIntent::Bulk,
            )
        }));

        let response = match response {
            Ok(response) => response,
            Err(_) => {
                crate::logging::log_warn(
                    "report",
                    format!(
                        "analysis panicked for {}; skipping file in report",
                        source_path.display()
                    ),
                );
                return Vec::new();
            }
        };

        response
            .imports
            .into_iter()
            .map(|item| crate::report::model::WorkspaceReportItem {
                source_file: source_path.to_string_lossy().to_string(),
                workspace_root: request.workspace_root.clone(),
                warning: if item.result.is_some() {
                    None
                } else {
                    item.message.clone()
                },
                detected: item.detected,
                // The report holds every row until all files finish and never reads the
                // per-module contributions, which are sized to the build graph.
                result: item.result.map(|mut result| {
                    result.internal_contributions = Vec::new();
                    result
                }),
            })
            .collect()
    }

    pub fn spawn_workspace_report(
        self: &std::sync::Arc<Self>,
        request: WorkspaceReportRequest,
        tx: tokio::sync::oneshot::Sender<WorkspaceReportResponse>,
    ) {
        let service = std::sync::Arc::clone(self);
        self.report_executor.spawn(move || {
            let _ = tx.send(service.build_workspace_report_on_worker(request));
        });
    }

    /// Analyze a document without waiting for any engine build (the editor's path).
    ///
    /// The response carries every import the cache answered plus a `Loading` placeholder for
    /// each one still to build, and returns at once. The pending builds come back in
    /// [`StreamedDocumentAnalysis::pending`]; the IPC server runs them and pushes each result
    /// (`RefreshedResults`), so a package that parks the bundler delays only its own number.
    ///
    /// The placeholder is load-bearing: the extension rebuilds its state array from
    /// `response.imports` (`listener.ts`) and a push can only update an existing state
    /// (`refreshMerge.ts`), so an omitted import would be lost permanently.
    pub fn handle_analyze_document_streaming(
        &self,
        request: AnalyzeDocumentRequest,
        ignore_resolver: &IgnoreRuleResolver,
    ) -> StreamedDocumentAnalysis {
        if !is_supported_protocol_version(request.version) {
            return StreamedDocumentAnalysis::settled(protocol_error_analyze_document_response(
                &request,
                format!("unsupported protocol version {}", request.version),
            ));
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let detected = match detected_imports_for_document(
            &request.active_document_path,
            &request.source,
            true,
            ignore_resolver,
        ) {
            Ok(imports) => imports,
            Err(error) => {
                return StreamedDocumentAnalysis::settled(AnalyzeDocumentResponse {
                    version: request.version,
                    request_id: request.request_id,
                    imports: Vec::new(),
                    error: Some(error.clone()),
                    diagnostics: vec![ImportDiagnostic::for_stage("document_parse", &error)],
                });
            }
        };
        let cached = self.cached_analysis_items_for_detected(
            &context,
            detected,
            false,
            ReadIntent::Interactive,
        );

        StreamedDocumentAnalysis {
            measured: cached.measured(),
            response: AnalyzeDocumentResponse {
                version: request.version,
                request_id: request.request_id,
                imports: cached.items,
                error: None,
                diagnostics: Vec::new(),
            },
            pending: cached.pending,
        }
    }

    fn handle_analyze_document_with_intent(
        &self,
        request: AnalyzeDocumentRequest,
        ignore_resolver: &IgnoreRuleResolver,
        intent: ReadIntent,
    ) -> AnalyzeDocumentResponse {
        if !is_supported_protocol_version(request.version) {
            return AnalyzeDocumentResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                imports: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let detected = match detected_imports_for_document(
            &request.active_document_path,
            &request.source,
            true,
            ignore_resolver,
        ) {
            Ok(imports) => imports,
            Err(error) => {
                return AnalyzeDocumentResponse {
                    version: request.version,
                    request_id: request.request_id,
                    imports: Vec::new(),
                    error: Some(error.clone()),
                    diagnostics: vec![ImportDiagnostic::for_stage("document_parse", &error)],
                };
            }
        };
        let imports = self.analysis_items_for_detected(&context, detected, false, intent);

        AnalyzeDocumentResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn handle_analyze_specifiers(
        &self,
        request: AnalyzeSpecifiersRequest,
    ) -> AnalyzeSpecifiersResponse {
        if !is_supported_protocol_version(request.version) {
            return AnalyzeSpecifiersResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                imports: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let detected = request
            .specifiers
            .iter()
            .filter(|specifier| is_runtime_package_specifier(specifier))
            .map(|specifier| detected_import_for_specifier(specifier))
            .collect::<Vec<_>>();
        let imports =
            self.analysis_items_for_detected(&context, detected, false, ReadIntent::Interactive);

        AnalyzeSpecifiersResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    /// Size a document and WAIT for every import's build.
    ///
    /// `importlens check` is the caller that needs this: it forces fresh, judges a byte budget,
    /// and a partial answer would pass a budget it should have failed. The editor takes
    /// [`Self::handle_file_size_document_streaming`].
    pub fn handle_file_size_document(
        &self,
        request: FileSizeDocumentRequest,
    ) -> FileSizeDocumentResponse {
        let (context, detected) = match file_size_document_prelude(&request) {
            Ok(prelude) => prelude,
            Err(response) => return *response,
        };
        // Serves stale (SWR) unless forced fresh; a stale serve triggers the background
        // revalidation and `RefreshedResults` push in the FileSizeDocument handler.
        let states = self.analysis_items_for_detected(
            &context,
            detected,
            !request.force_fresh,
            ReadIntent::Interactive,
        );

        self.file_size_document_response(&request, &context, states)
    }

    /// Size a document without waiting for any per-import build.
    ///
    /// The file's totals still come from one combined build per runtime, bounded by
    /// `BUILD_TIMEOUT`, whose entries include imports still being measured individually. Those
    /// individual states come back `Loading`; `AnalyzeDocument`'s streaming pass (which the
    /// extension always sends first, for the same document and generation) builds and pushes
    /// them.
    ///
    /// A force-fresh request (CI) takes the blocking path: completeness is that flag's point.
    pub fn handle_file_size_document_streaming(
        &self,
        request: FileSizeDocumentRequest,
    ) -> FileSizeDocumentResponse {
        if request.force_fresh {
            return self.handle_file_size_document(request);
        }

        let (context, detected) = match file_size_document_prelude(&request) {
            Ok(prelude) => prelude,
            Err(response) => return *response,
        };
        let cached = self.cached_analysis_items_for_detected(
            &context,
            detected,
            true,
            ReadIntent::Interactive,
        );

        self.file_size_document_response(&request, &context, cached.items)
    }

    fn file_size_document_response(
        &self,
        request: &FileSizeDocumentRequest,
        context: &AnalysisContext,
        states: Vec<ImportAnalysisItem>,
    ) -> FileSizeDocumentResponse {
        // Every detected import reaches the aggregate, including one with no `request` (no
        // installed version): it is a floor, like every unmeasured contributor (FR-024a).
        //
        // Except a path alias (`@app/components`, a bare specifier under `baseUrl`): it points at
        // first-party source, which is not measured (ADR-0004), so its zero is a fact, not a gap.
        // The discriminator is positive evidence (it resolves through tsconfig `paths` to
        // first-party source). A specifier that resolves to nothing is a floor, declared or not:
        // a typo and an uninstalled dependency omit the same bytes (ADR-0006).
        //
        // One probe for the whole loop: it holds one alias resolver per reachable tsconfig, and
        // building them per specifier cost ~20 ms of the 50 ms NFR-002 warm budget on a 20-alias
        // component. It builds lazily, on the first import with no request.
        //
        // It must not outlive this response: a surviving `Resolver` memoizes the filesystem, and
        // a memoized miss would keep an import written before its target file a floor for the
        // daemon's life. It is keyed on the workspace, not
        // the document, so the answer does not depend on the importing file's extension.
        let first_party =
            FirstPartySourceProbe::new(&context.workspace_root, &context.active_document_path);
        let sized = states
            .iter()
            .map(|state| match state.request.clone() {
                Some(request) => SizedImport::installed(request, state.result.clone()),
                None if first_party.resolves_to_first_party_source(&state.detected.specifier) => {
                    SizedImport::path_alias(state.detected.specifier.clone())
                }
                None => SizedImport::not_installed(state.detected.specifier.clone()),
            })
            .collect::<Vec<_>>();
        let results = states
            .iter()
            .filter_map(|state| state.result.clone())
            .collect::<Vec<_>>();
        let file_size = self.file_size_with_cache(context, &request.active_document_path, &sized);

        FileSizeDocumentResponse {
            version: request.version,
            request_id: request.request_id,
            raw_bytes: file_size.raw_bytes,
            minified_bytes: file_size.minified_bytes,
            gzip_bytes: file_size.gzip_bytes,
            brotli_bytes: file_size.brotli_bytes,
            zstd_bytes: file_size.zstd_bytes,
            imports: results,
            states,
            // What the five totals are made of; already inside them.
            asset_breakdown: file_size.asset_breakdown.clone(),
            // Whether every import was really measured. The extension keeps a floor out of its
            // persisted bundle-impact history (FR-026c), a store with no TTL.
            incomplete: file_size.incomplete,
            // Whether the file's own combined build failed. It can fail with every contributor
            // measured; the totals are then an un-deduplicated per-import sum (an over-count).
            degraded: file_size.degraded,
            error: file_size.error,
            diagnostics: file_size.diagnostics,
        }
    }

    /// Background SWR revalidation: recomputes fresh only the imports in `stale_specifiers`
    /// (a fresh sibling is never re-analyzed), deduped per key by a `RevalidationGuard` so
    /// concurrent stale serves coalesce and a panic cannot leak the claim. `should_continue`
    /// bails a superseded document before each recompute. A key still `Stale` after its
    /// recompute gets exactly one more (never a loop).
    ///
    /// Returns `(workspace_root, document_path, results, identities)` for the push, or `None`
    /// when nothing was recomputed.
    ///
    /// No daemon-side debounce: the §4.5 debounce lives in the client's
    /// `DebouncedDocumentScheduler` (`extension/src/listener.ts`, per document, cancel and
    /// replace), and the guard coalesces overlapping requests for one key.
    pub fn revalidate_document_sizes(
        &self,
        request: &FileSizeDocumentRequest,
        stale_specifiers: &HashSet<String>,
        should_continue: impl Fn() -> bool,
    ) -> Option<(
        String,
        String,
        Vec<ImportResult>,
        Vec<RefreshedImportIdentity>,
    )> {
        if stale_specifiers.is_empty() {
            return None;
        }
        if !should_continue() {
            return None;
        }
        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let ignore_resolver = IgnoreRuleResolver::default();
        let detected = detected_imports_for_document(
            &request.active_document_path,
            &request.source,
            true,
            &ignore_resolver,
        )
        .ok()?;

        let cache = self.cache_registry.cache_for_root(&context.workspace_root);
        let mut fresh = Vec::new();
        // Index-aligned with `fresh`, so the client assigns each result to the right
        // same-specifier variant.
        let mut identities = Vec::new();
        for detected_import in &detected {
            if !stale_specifiers.contains(&detected_import.specifier) {
                continue;
            }
            if !should_continue() {
                break;
            }
            // Not via `analysis_items_for_detected`: that would recompute every import before
            // the dedupe gate below could fire.
            let Ok(import_request) =
                import_request_for_detected(&context.active_document_path, detected_import)
            else {
                continue;
            };
            let Ok(resolved) =
                resolve_package_entry(&context.active_document_path, &import_request)
            else {
                continue;
            };
            let key = cache_key_for_resolved_import(&import_request, &resolved);
            // A served-`Stale` specifier is a real change or a transient `Unknown` graduated to
            // `Stale{revalidating}` (§4.3.1). Never recompute an `Unknown`: it would hit the same
            // transient error and could overwrite the good cached value. The re-probe re-stats
            // the dependency, which heals a graduated key on a later get.
            let freshness = cache.probe_freshness(&key);
            if matches!(freshness, Some(crate::cache::key::Freshness::Unknown)) {
                continue;
            }
            // The claim is scoped to one document generation, so another document importing the
            // same package still gets its own refresh push.
            let claim_key = revalidation_claim_key(
                &key,
                &request.workspace_root,
                &request.active_document_path,
                request.analysis_generation,
            );
            let Some(_guard) = cache.begin_revalidation(&claim_key) else {
                continue;
            };
            // `Fresh` again: another request rebuilt the entry after this one was served
            // stale, so the client is owed that value, not a second build of it.
            let healed = matches!(freshness, Some(crate::cache::key::Freshness::Fresh))
                .then(|| cache.get_if_fresh_and_promote(&key))
                .flatten();
            let mut result = match healed {
                Some(result) => result,
                None => self.analyze_and_cache(
                    cache.as_ref(),
                    &context,
                    &import_request,
                    key.clone(),
                    resolved.clone(),
                    || true,
                ),
            };
            // Trailing re-check (see `should_rearm_revalidation`): one more recompute at most; a
            // still-`Stale` second result is left for the next interactive read.
            if should_rearm_revalidation(cache.probe_freshness(&key)) {
                result = self.analyze_and_cache(
                    cache.as_ref(),
                    &context,
                    &import_request,
                    key.clone(),
                    resolved,
                    || true,
                );
            }
            if !should_continue() {
                break;
            }
            if !should_cache_result(&result) {
                continue;
            }
            fresh.push(result);
            identities.push(RefreshedImportIdentity {
                specifier: detected_import.specifier.clone(),
                import_kind: detected_import.import_kind,
                named: detected_import.named.clone(),
                runtime: detected_import.runtime,
            });
        }

        if fresh.is_empty() {
            return None;
        }
        Some((
            request.workspace_root.clone(),
            request.active_document_path.clone(),
            fresh,
            identities,
        ))
    }

    pub fn handle_analyze_package_json(
        &self,
        request: AnalyzePackageJsonRequest,
    ) -> AnalyzePackageJsonResponse {
        self.analyze_package_json(request, None::<fn(AnalyzePackageJsonResponse)>)
    }

    pub fn handle_analyze_package_json_streaming<F>(
        &self,
        request: AnalyzePackageJsonRequest,
        emit_partial: F,
    ) -> AnalyzePackageJsonResponse
    where
        F: Fn(AnalyzePackageJsonResponse) + Sync,
    {
        let streaming = request.streaming;
        self.analyze_package_json(request, streaming.then_some(emit_partial))
    }

    fn analyze_package_json<F>(
        &self,
        request: AnalyzePackageJsonRequest,
        emit_partial: Option<F>,
    ) -> AnalyzePackageJsonResponse
    where
        F: Fn(AnalyzePackageJsonResponse) + Sync,
    {
        let request_started_at = Instant::now();
        if !is_supported_protocol_version(request.version) {
            return AnalyzePackageJsonResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                sections: Vec::new(),
                states: Vec::new(),
                indexes: None,
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let sections = package_json_dependency_sections(&request.source);
        let registry_hint_mode = effective_registry_hint_mode(&request);
        let now_ms = crate::time::unix_millis_now();
        let entries = package_json_dependency_entries(&request.source);
        crate::logging::log_debug(
            "package_json",
            format!(
                "request {} parsed {} dependencies across {} section(s) in {}ms (source_chars={}, registry_mode={:?})",
                request.request_id,
                entries.len(),
                sections.len(),
                request_started_at.elapsed().as_millis(),
                request.source.len(),
                registry_hint_mode
            ),
        );

        if let Some(emit_partial) = emit_partial.as_ref()
            && !entries.is_empty()
        {
            let loading_states = entries
                .iter()
                .map(|entry| PackageJsonDependencyAnalysisItem {
                    name: entry.name.clone(),
                    section: entry.section.clone(),
                    entry: entry.clone(),
                    status: ImportAnalysisStatus::Loading,
                    installed_version: None,
                    registry_hint: None,
                    message: None,
                    result: None,
                })
                .collect::<Vec<_>>();
            emit_partial(AnalyzePackageJsonResponse {
                version: request.version,
                request_id: request.request_id,
                sections: sections.clone(),
                states: loading_states,
                indexes: Some((0..entries.len()).collect()),
                error: None,
                diagnostics: Vec::new(),
            });
            crate::logging::log_debug(
                "package_json",
                format!(
                    "request {} emitted loading partial for {} dependencies after {}ms",
                    request.request_id,
                    entries.len(),
                    request_started_at.elapsed().as_millis()
                ),
            );
        }

        // Resolves each dependency in parallel; `into_par_iter` preserves order, so states and
        // import_requests line up with the streaming indexes.
        type PreparedDependency = (ImportRequest, Result<ResolvedPackage, String>);
        let resolution_started_at = Instant::now();
        let resolved: Vec<(
            PackageJsonDependencyAnalysisItem,
            Option<PreparedDependency>,
        )> = entries
            .into_par_iter()
            .map(|entry| {
                // Resolved once and carried to the analysis pass, so the manifest is read once.
                // An installed-but-unresolvable package (e.g. types-only) falls back to the
                // lightweight version read and carries the resolver's message, which settles
                // it without a build.
                let probe = ImportRequest {
                    specifier: entry.name.clone(),
                    package_name: entry.name.clone(),
                    version: String::new(),
                    named: Vec::new(),
                    import_kind: ImportKind::Namespace,
                    runtime: ImportRuntime::Component,
                };
                let (version, resolved) =
                    match resolve_package_entry(&context.active_document_path, &probe) {
                        Ok(resolved) => {
                            let version = resolved
                                .package_json
                                .get("version")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_owned();
                            (Ok(version), Ok(resolved))
                        }
                        Err(message) => (
                            resolve_installed_package_version(
                                &context.active_document_path,
                                &entry.name,
                            ),
                            Err(message),
                        ),
                    };

                match version {
                    Ok(version) => {
                        let import_request = ImportRequest {
                            specifier: entry.name.clone(),
                            package_name: entry.name.clone(),
                            version: version.clone(),
                            named: Vec::new(),
                            import_kind: ImportKind::Namespace,
                            runtime: ImportRuntime::Component,
                        };
                        let registry_hint = self
                            .registry_hints
                            .hint_for(&entry.name, Some(&version), registry_hint_mode, now_ms)
                            .hint;
                        let state = PackageJsonDependencyAnalysisItem {
                            name: entry.name.clone(),
                            section: entry.section.clone(),
                            entry,
                            status: ImportAnalysisStatus::Loading,
                            installed_version: Some(version),
                            registry_hint,
                            message: None,
                            result: None,
                        };
                        (state, Some((import_request, resolved)))
                    }
                    Err(message) => {
                        let state = PackageJsonDependencyAnalysisItem {
                            name: entry.name.clone(),
                            section: entry.section.clone(),
                            entry,
                            status: ImportAnalysisStatus::Missing,
                            installed_version: None,
                            registry_hint: None,
                            message: Some(message),
                            result: None,
                        };
                        (state, None)
                    }
                }
            })
            .collect();
        crate::logging::log_debug(
            "package_json",
            format!(
                "request {} resolved dependency metadata in {}ms",
                request.request_id,
                resolution_started_at.elapsed().as_millis()
            ),
        );
        let (mut states, import_requests): (Vec<_>, Vec<_>) = resolved.into_iter().unzip();
        // Persist any registry metadata fetched above in one snapshot write.
        self.registry_hints.flush();

        if let Some(emit_partial) = emit_partial.as_ref()
            && !states.is_empty()
        {
            emit_partial(AnalyzePackageJsonResponse {
                version: request.version,
                request_id: request.request_id,
                sections: sections.clone(),
                states: states.clone(),
                indexes: Some((0..states.len()).collect()),
                error: None,
                diagnostics: Vec::new(),
            });
            crate::logging::log_debug(
                "package_json",
                format!(
                    "request {} emitted resolved partial for {} dependencies after {}ms",
                    request.request_id,
                    states.len(),
                    request_started_at.elapsed().as_millis()
                ),
            );
        }

        struct PendingPackageJsonAnalysis {
            import_request: ImportRequest,
            resolved: ResolvedPackage,
            cache_key: String,
        }

        // Settled: a fresh cache hit, or a package that does not resolve (answered without a
        // build). Only a real miss is Pending and queues for an engine permit.
        enum PackageJsonCacheClassification {
            Settled {
                index: usize,
                result: ImportResult,
            },
            Pending {
                index: usize,
                analysis: PendingPackageJsonAnalysis,
            },
        }

        let package_cache = self.cache_registry.cache_for_root(&context.workspace_root);
        let classifications = import_requests
            .par_iter()
            .enumerate()
            .filter_map(|(index, prepared)| {
                let (import_request, resolved) = prepared.as_ref()?;

                let resolved = match resolved {
                    Ok(resolved) => resolved,
                    Err(message) => {
                        return Some(PackageJsonCacheClassification::Settled {
                            index,
                            result: analyze_unresolved_import(
                                &context,
                                import_request,
                                message.clone(),
                            ),
                        });
                    }
                };

                let (cache_key, cached_result) = fresh_cached_result_for_resolved_import(
                    package_cache.as_ref(),
                    import_request,
                    resolved,
                    ReadIntent::Interactive,
                );
                if let Some(result) = cached_result {
                    return Some(PackageJsonCacheClassification::Settled { index, result });
                }

                Some(PackageJsonCacheClassification::Pending {
                    index,
                    analysis: PendingPackageJsonAnalysis {
                        import_request: import_request.clone(),
                        resolved: resolved.clone(),
                        cache_key,
                    },
                })
            })
            .collect::<Vec<_>>();
        let mut cached_indexed_results = Vec::new();
        let mut pending_analysis = Vec::new();
        for classification in classifications {
            match classification {
                PackageJsonCacheClassification::Settled { index, result } => {
                    cached_indexed_results.push((index, result));
                }
                PackageJsonCacheClassification::Pending { index, analysis } => {
                    pending_analysis.push((index, analysis));
                }
            }
        }
        cached_indexed_results.sort_by_key(|(index, _)| *index);
        pending_analysis.sort_by_key(|(index, _)| *index);

        if let Some(emit_partial) = emit_partial.as_ref()
            && !cached_indexed_results.is_empty()
        {
            let mut indexes = Vec::with_capacity(cached_indexed_results.len());
            let mut cached_states = Vec::with_capacity(cached_indexed_results.len());
            for (index, result) in &cached_indexed_results {
                let mut state = states[*index].clone();
                state.status = ImportAnalysisStatus::Ready;
                state.result = Some(result.clone());
                indexes.push(*index);
                cached_states.push(state);
            }
            emit_partial(AnalyzePackageJsonResponse {
                version: request.version,
                request_id: request.request_id,
                sections: Vec::new(),
                states: cached_states,
                indexes: Some(indexes),
                error: None,
                diagnostics: Vec::new(),
            });
            crate::logging::log_debug(
                "package_json",
                format!(
                    "request {} emitted cached size partial for {} dependencies after {}ms",
                    request.request_id,
                    cached_indexed_results.len(),
                    request_started_at.elapsed().as_millis()
                ),
            );
        }

        let analysis_started_at = Instant::now();
        let analyzed_results = drain_ordered_owned(pending_analysis, |_, (index, pending)| {
            let result = self.analyze_and_cache(
                package_cache.as_ref(),
                &context,
                &pending.import_request,
                pending.cache_key,
                pending.resolved,
                || true,
            );

            if let Some(emit_partial) = emit_partial.as_ref() {
                let mut state = states[index].clone();
                state.status = ImportAnalysisStatus::Ready;
                state.result = Some(result.clone());
                emit_partial(AnalyzePackageJsonResponse {
                    version: request.version,
                    request_id: request.request_id,
                    sections: Vec::new(),
                    states: vec![state],
                    indexes: Some(vec![index]),
                    error: None,
                    diagnostics: Vec::new(),
                });
            }

            (index, result)
        });
        let mut indexed_results = cached_indexed_results;
        indexed_results.extend(analyzed_results);
        indexed_results.sort_by_key(|(index, _)| *index);
        let cache_hits = indexed_results
            .iter()
            .filter(|(_, result)| result.cache_hit)
            .count();
        let stale_results = indexed_results
            .iter()
            .filter(|(_, result)| matches!(result.freshness.kind, FreshnessKind::Stale))
            .count();
        let unverified_results = indexed_results
            .iter()
            .filter(|(_, result)| matches!(result.freshness.kind, FreshnessKind::Unverified))
            .count();
        crate::logging::log_debug(
            "package_json",
            format!(
                "request {} analyzed {} dependencies in {}ms (cache_hits={}/{}, stale={}, unverified={})",
                request.request_id,
                indexed_results.len(),
                analysis_started_at.elapsed().as_millis(),
                cache_hits,
                indexed_results.len(),
                stale_results,
                unverified_results
            ),
        );
        let (indexes, mut results): (Vec<_>, Vec<_>) = indexed_results.into_iter().unzip();
        // A `package.json` dependency has no runtime split, but the runtime still comes from its
        // request so the partition has one source.
        let runtimes = indexes
            .iter()
            .map(|index| {
                import_requests[*index]
                    .as_ref()
                    .map(|(request, _)| request.runtime)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        annotate_shared_bytes(runtimes.into_iter().zip(results.iter_mut()));
        for (index, result) in indexes.into_iter().zip(results) {
            states[index].status = ImportAnalysisStatus::Ready;
            states[index].result = Some(result);
        }

        crate::logging::log_debug(
            "package_json",
            format!(
                "request {} completed in {}ms (states={})",
                request.request_id,
                request_started_at.elapsed().as_millis(),
                states.len()
            ),
        );

        AnalyzePackageJsonResponse {
            version: request.version,
            request_id: request.request_id,
            sections,
            states,
            indexes: None,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn complete_import_members(
        &self,
        request: CompleteImportMembersRequest,
    ) -> CompleteImportMembersResponse {
        if !(2..=PROTOCOL_VERSION).contains(&request.version) {
            return CompleteImportMembersResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                specifier: None,
                exports: Vec::new(),
                imported_names: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        let Some(context) = named_import_completion_context(
            &request.active_document_path,
            &request.source,
            request.cursor_offset,
        ) else {
            return CompleteImportMembersResponse {
                version: request.version,
                request_id: request.request_id,
                specifier: None,
                exports: Vec::new(),
                imported_names: Vec::new(),
                error: None,
                diagnostics: Vec::new(),
            };
        };

        let package_name = get_package_name(&context.specifier);
        let package_version = match resolve_installed_package_version(
            Path::new(&request.active_document_path),
            &package_name,
        ) {
            Ok(version) => version,
            Err(error) => {
                return CompleteImportMembersResponse {
                    version: request.version,
                    request_id: request.request_id,
                    specifier: Some(context.specifier),
                    exports: Vec::new(),
                    imported_names: context.imported_names,
                    error: Some(error.clone()),
                    diagnostics: vec![ImportDiagnostic::for_stage("package_resolution", &error)],
                };
            }
        };

        // The runtime is already classified from the live buffer by
        // `named_import_completion_context`, so it is passed through, not re-derived from disk
        // (hence `cursor_offset: None`).
        let response = self.enumerate_exports_with_runtime(
            EnumerateExportsRequest {
                message_type: "enumerate_exports".to_owned(),
                version: request.version,
                request_id: request.request_id,
                workspace_root: request.workspace_root,
                active_document_path: request.active_document_path,
                specifier: context.specifier.clone(),
                package_name,
                package_version,
                cursor_offset: None,
            },
            context.runtime,
        );

        CompleteImportMembersResponse {
            version: response.version,
            request_id: response.request_id,
            specifier: Some(context.specifier),
            exports: response.exports,
            imported_names: context.imported_names,
            error: response.error,
            diagnostics: response.diagnostics,
        }
    }

    pub fn enumerate_exports(&self, request: EnumerateExportsRequest) -> EnumerateExportsResponse {
        if !(2..=PROTOCOL_VERSION).contains(&request.version) {
            return EnumerateExportsResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                specifier: request.specifier,
                exports: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: Vec::new(),
            };
        }

        let runtime = runtime_for_enumeration(&request);
        self.enumerate_exports_with_runtime(request, runtime)
    }

    /// The enumeration, once the runtime is decided by the document classifier (from the live
    /// buffer for completion, from the cursor offset for a direct request). Never hardcode it:
    /// the runtime drives both resolution (`browser` vs `node` conditions) and the memo key.
    fn enumerate_exports_with_runtime(
        &self,
        request: EnumerateExportsRequest,
        runtime: ImportRuntime,
    ) -> EnumerateExportsResponse {
        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let import_request = ImportRequest {
            specifier: request.specifier.clone(),
            package_name: request.package_name,
            version: request.package_version,
            named: Vec::new(),
            import_kind: ImportKind::Namespace,
            runtime,
        };

        let resolved = match resolve_package_entry(&context.active_document_path, &import_request) {
            Ok(resolved) => resolved,
            Err(error) => {
                return EnumerateExportsResponse {
                    version: request.version,
                    request_id: request.request_id,
                    specifier: request.specifier,
                    exports: Vec::new(),
                    error: Some(error.clone()),
                    diagnostics: vec![ImportDiagnostic {
                        stage: "entry_resolution".to_owned(),
                        message: error,
                        details: Vec::new(),
                    }],
                };
            }
        };

        match crate::pipeline::export_list::enumerate_exports_cached(
            &context,
            &resolved.package_root,
            &resolved.entry_path,
            import_request.runtime,
        ) {
            Ok(enumeration) => EnumerateExportsResponse {
                version: request.version,
                request_id: request.request_id,
                specifier: request.specifier,
                exports: enumeration.names,
                error: None,
                diagnostics: enumeration
                    .diagnostics
                    .into_iter()
                    .map(|diagnostic| ImportDiagnostic {
                        stage: diagnostic.stage,
                        message: diagnostic.message,
                        details: Vec::new(),
                    })
                    .collect(),
            },
            Err(failure) => EnumerateExportsResponse {
                version: request.version,
                request_id: request.request_id,
                specifier: request.specifier,
                exports: Vec::new(),
                error: Some(failure.message.clone()),
                diagnostics: vec![ImportDiagnostic {
                    stage: failure.stage,
                    message: failure.message,
                    details: Vec::new(),
                }],
            },
        }
    }

    pub fn cache_status(&self, request: CacheStatusRequest) -> CacheStatusResponse {
        if !is_supported_protocol_version(request.version) {
            return CacheStatusResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                total_size_bytes: 0,
                project_count: 0,
                max_size_mb: 0,
                current_project: None,
                total_bytes: 0,
                budget_bytes: 0,
                registry_size_bytes: 0,
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        let project_root = request.workspace_root.as_deref().map(Path::new);
        let status = self.cache_registry.status_for_root(project_root);

        CacheStatusResponse {
            version: request.version,
            request_id: request.request_id,
            total_size_bytes: status.total_size_bytes,
            project_count: status.project_count,
            max_size_mb: status.max_size_mb,
            current_project: status.current_project,
            total_bytes: status.total_bytes,
            budget_bytes: status.budget_bytes,
            // One serialized-length measurement of the shared registry snapshot, not a scan.
            registry_size_bytes: self.registry_hints.registry_size_bytes(),
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn list_cache(&self, request: CacheListRequest) -> CacheListResponse {
        if !is_supported_protocol_version(request.version) {
            return CacheListResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                shards: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        CacheListResponse {
            version: request.version,
            request_id: request.request_id,
            shards: self.cache_registry.list_shards(),
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn remove_cache(&self, request: CacheRemoveRequest) -> CacheRemoveResponse {
        if !is_supported_protocol_version(request.version) {
            return CacheRemoveResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                removed: Vec::new(),
                failed: Vec::new(),
                scrubbed_entries: 0,
                registry_entries_removed: 0,
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        // The orphan purge's non-shard work, reported so the UI does not say "nothing to
        // reclaim" after reclaiming entries.
        let mut scrubbed_entries = 0usize;
        let mut registry_entries_removed = 0usize;

        let results = match request.scope {
            CacheRemoveScope::CurrentProject => match request.workspace_root.as_deref() {
                Some(project_root) => self
                    .cache_registry
                    .remove_current_project(Path::new(project_root)),
                None => {
                    return CacheRemoveResponse {
                        version: request.version,
                        request_id: request.request_id,
                        removed: Vec::new(),
                        failed: Vec::new(),
                        scrubbed_entries: 0,
                        registry_entries_removed: 0,
                        error: Some(
                            "workspace_root is required for current_project cache removal"
                                .to_owned(),
                        ),
                        diagnostics: vec![ImportDiagnostic::for_stage(
                            "protocol",
                            "workspace_root is required for current_project cache removal",
                        )],
                    };
                }
            },
            CacheRemoveScope::Selected => self
                .cache_registry
                .remove_selected(request.shard_ids.as_deref().unwrap_or(&[])),
            CacheRemoveScope::All => {
                let removed = self.cache_registry.remove_all();
                // "Clear everything" also drops the npm-hint store and the shared resolver
                // caches; the L1/graph caches are cleared unconditionally below.
                self.registry_hints.clear();
                crate::pipeline::resolver::invalidate_shared_resolvers();
                removed
            }
            CacheRemoveScope::Registry => {
                // Registry-only: bundle shards and their L1/graph caches stay put.
                self.registry_hints.clear();
                Vec::new()
            }
            CacheRemoveScope::Orphans => {
                // Manual "Remove Orphaned Caches": drive-safe shard reclaim for moved/deleted
                // projects, a stale-entry scrub of surviving shards, and a stale registry
                // prune. The maintenance pass runs the shard reclaim automatically (throttled).
                registry_entries_removed = self.registry_hints.purge_expired_metadata();
                if registry_entries_removed > 0 {
                    crate::logging::log_debug(
                        "registry",
                        format!(
                            "orphan purge dropped {registry_entries_removed} stale registry entries"
                        ),
                    );
                }
                let (removed_shards, scrubbed) = self.cache_registry.purge_orphans();
                scrubbed_entries = scrubbed;
                removed_shards
            }
        };
        let (removed, failed): (Vec<_>, Vec<_>) =
            results.into_iter().partition(|result| result.removed);

        if matches!(request.scope, CacheRemoveScope::Orphans) {
            // An entry-only purge removes no shard, so the blanket clear below does not fire;
            // drop the L1/graph entries whose paths are gone.
            crate::pipeline::file_size_cache::shared_file_size_cache().purge_missing_paths();
            crate::engine::dependency_paths::purge_missing();
        }

        // `All` drops the derived caches even when it removed no shard; other scopes only when
        // a shard was removed.
        if matches!(request.scope, CacheRemoveScope::All) || !removed.is_empty() {
            crate::engine::dependency_paths::clear();
            // The L1 aggregates too, so the status-bar size recomputes after a clear.
            crate::pipeline::file_size_cache::shared_file_size_cache().clear();
        }

        // An in-flight analysis that captured the pre-clear generation must not repopulate the
        // store as fresh: its insert lands `verified_generation < current` and re-validates.
        crate::cache::memory::bump_cache_generation();

        CacheRemoveResponse {
            version: request.version,
            request_id: request.request_id,
            removed,
            failed,
            scrubbed_entries,
            registry_entries_removed,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.cache_registry.invalidate_package(package_name);
        crate::engine::dependency_paths::invalidate_package(package_name);
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
    }

    pub fn invalidate_all(&self) {
        self.cache_registry.clear_all();
        crate::engine::dependency_paths::clear();
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
    }

    /// One cache-maintenance pass: evict least-recently-used entries across shards to the global
    /// disk-byte budget, compact fragmented shard files, apply registry retention and size cap,
    /// and sweep orphaned shards. Scheduled once per Hello, after a delay, on `spawn_blocking`
    /// (decision-log D3). Registry retention runs here so it stays off the write hot path.
    pub fn run_cache_maintenance(&self) {
        // A concurrent pass (e.g. from a previous connection) makes this one a no-op.
        let Some(_maintenance_guard) = try_begin_cache_maintenance() else {
            return;
        };
        let outcome = self.cache_registry.run_maintenance(false);
        if outcome.eviction.evicted_keys > 0 {
            crate::logging::log_debug(
                "cache",
                format!(
                    "byte-budget eviction freed {} bytes across {} entries",
                    outcome.eviction.evicted_bytes, outcome.eviction.evicted_keys
                ),
            );
        }
        if outcome.compacted_shards > 0 {
            crate::logging::log_debug(
                "cache",
                format!("compacted {} shard file(s)", outcome.compacted_shards),
            );
        }

        // Registry store retention and byte-budget cap, written authoritatively so deletions
        // stick. The budget is `importLens.registryCacheMaxSizeMB` from Hello; a client that
        // omits it gets the daemon default (serde-defaulted in `HelloMessage`).
        let registry_removed = self.registry_hints.run_maintenance(
            crate::time::unix_millis_now(),
            self.registry_cache_max_size_bytes,
        );
        if registry_removed > 0 {
            crate::logging::log_debug(
                "registry",
                format!("maintenance dropped {registry_removed} stale/over-cap registry entries"),
            );
        }

        // Orphaned shards: a moved/deleted project is never reopened, so on-access reclaim never
        // reaches its shard. Drive-safe (an unplugged drive keeps its shard) and throttled by
        // `ORPHAN_SWEEP_INTERVAL`. Removing a shard strands its L1/graph entries, so clear them
        // and bump the generation, as `remove_cache` does.
        let orphans_removed = self
            .cache_registry
            .sweep_orphaned_shards_if_due()
            .iter()
            .filter(|result| result.removed)
            .count();
        if orphans_removed > 0 {
            crate::engine::dependency_paths::clear();
            crate::pipeline::file_size_cache::shared_file_size_cache().clear();
            crate::cache::memory::bump_cache_generation();
            crate::logging::log_info(
                "cache",
                format!("reclaimed {orphans_removed} orphaned project cache shard(s)"),
            );
        }
    }

    pub fn recent_cache_keys(&self, workspace_root: &Path, limit: usize) -> Vec<String> {
        self.cache_registry.recent_keys(workspace_root, limit)
    }

    pub fn flush_cache(&self) -> Result<(), String> {
        self.cache_registry.flush_to_disk()
    }

    pub fn prewarm_resolved_import<F>(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        resolved: ResolvedPackage,
        should_continue: F,
    ) where
        F: Fn() -> bool,
    {
        let key = cache_key_for_resolved_import(request, &resolved);
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);

        // A prewarm read must not promote recency (§5.1).
        if cache.get_for_prewarm(&key).is_some() || !should_continue() {
            return;
        }

        let _ = self.analyze_and_cache(
            cache.as_ref(),
            context,
            request,
            key,
            resolved,
            should_continue,
        );
    }

    pub fn invalidate_package_json_paths(&self, package_json_paths: &[String]) -> bool {
        let mut package_names = Vec::with_capacity(package_json_paths.len());
        for package_json_path in package_json_paths {
            match package_name_from_package_json_path(package_json_path) {
                Some(package_name) => package_names.push(package_name),
                // One unmappable path (pnpm's `.pnpm/…` store, a symlinked package) is no
                // reason to clear every other project's cache: skip it.
                None => crate::logging::log_debug(
                    "cache",
                    format!(
                        "unmappable package.json path, skipping targeted invalidation: {package_json_path}"
                    ),
                ),
            }
        }

        if package_names.is_empty() {
            // An empty batch is a no-op; a batch of only unmappable paths has no safe targeted
            // fallback, so it clears everything.
            if package_json_paths.is_empty() {
                return false;
            }
            self.invalidate_all();
            return true;
        }

        // Targeted even for a large burst: a full clear would evict unrelated sibling projects
        // in a multi-root window. The resolver and generation invalidations run once per burst.
        self.cache_registry.invalidate_packages(&package_names);
        for package_name in &package_names {
            crate::engine::dependency_paths::invalidate_package(package_name);
        }
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
        true
    }

    /// A `tsconfig.json` / `jsconfig.json` changed on disk, so the workspace's alias table did.
    ///
    /// The alias resolvers are rebuilt per query, so a `paths` edit needs no message. What this
    /// drops is the memoized reachable-config walk (which projects the `references` graph
    /// reaches): a config that starts referencing the project owning the `paths` is invisible
    /// until it is dropped. It rides the `node_modules_changed` path to
    /// `invalidate_shared_resolvers`.
    ///
    /// No generation bump and no shard touched: a tsconfig never affects what a package weighs.
    /// The L1 aggregates are cleared, since a flipped alias classification changes which imports
    /// contribute bytes and whether the total is a floor.
    ///
    /// Returns whether anything was invalidated, so an empty batch stays a no-op.
    pub fn invalidate_workspace_config_paths(&self, config_paths: &[String]) -> bool {
        if config_paths.is_empty() {
            return false;
        }

        crate::logging::log_debug(
            "cache",
            format!(
                "workspace config changed ({} path(s)); dropping the shared resolvers and L1 aggregates",
                config_paths.len()
            ),
        );
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::pipeline::file_size_cache::shared_file_size_cache().clear();
        true
    }

    /// Settle every import answerable without an engine build, and mark the rest `Loading`.
    ///
    /// - a cache hit: `Ready`;
    /// - an import that does not resolve: settled by `analyze_unresolved_import`, which is
    ///   filesystem work only;
    /// - a real miss: `Loading`, carried out in [`StreamedDocumentAnalysis::pending`].
    ///
    /// The `Loading` item keeps its `request`, so a caller needing only the resolved package
    /// identity (the named-export candidates command) is unaffected.
    fn cached_analysis_items_for_detected(
        &self,
        context: &AnalysisContext,
        detected: Vec<DetectedImport>,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> CachedDocumentAnalysis {
        let classified = detected
            .into_par_iter()
            .map(|detected| {
                let request =
                    match import_request_for_detected(&context.active_document_path, &detected) {
                        Ok(request) => request,
                        Err(message) => {
                            return (
                                ImportAnalysisItem {
                                    detected,
                                    status: ImportAnalysisStatus::Missing,
                                    message: Some(message),
                                    request: None,
                                    result: None,
                                },
                                None,
                            );
                        }
                    };

                match self.probe_cache(context, &request, serve_stale, intent) {
                    CacheProbe::Hit(result) => (
                        ImportAnalysisItem {
                            detected,
                            status: ImportAnalysisStatus::Ready,
                            message: None,
                            request: Some(request),
                            result: Some(*result),
                        },
                        None,
                    ),
                    CacheProbe::Unresolved(message) => {
                        let result = analyze_unresolved_import(context, &request, message);
                        (
                            ImportAnalysisItem {
                                detected,
                                status: ImportAnalysisStatus::Ready,
                                message: None,
                                request: Some(request),
                                result: Some(result),
                            },
                            None,
                        )
                    }
                    CacheProbe::Miss(pending) => (
                        ImportAnalysisItem {
                            detected: detected.clone(),
                            status: ImportAnalysisStatus::Loading,
                            message: None,
                            request: Some(request.clone()),
                            result: None,
                        },
                        Some(PendingImport {
                            detected,
                            request,
                            pending,
                        }),
                    ),
                }
            })
            .collect::<Vec<_>>();

        let mut items = Vec::with_capacity(classified.len());
        let mut pending = Vec::new();
        for (item, work) in classified {
            items.push(item);
            if let Some(work) = work {
                pending.push(work);
            }
        }
        annotate_ready_items(&mut items);

        CachedDocumentAnalysis { items, pending }
    }

    /// Build the imports a streamed response answered `Loading`, handing each result to `emit`
    /// as it lands. Runs off the response path, so a build that parks for the full
    /// `BUILD_TIMEOUT` delays only its own import.
    ///
    /// `should_continue` is checked around each build so a superseded document stops early.
    /// Builds go through `analyze_and_cache`, so single-flight joins a build already running
    /// for the same key.
    ///
    /// `shared_bytes` is a relation between imports of the same file, knowable only once every
    /// import is measured. Each import is pushed as soon as it is measured, then one closing push
    /// carries the shared-byte corrections; without it a cold document would never show them.
    pub fn complete_pending_imports(
        &self,
        context: &AnalysisContext,
        measured: Vec<MeasuredImport>,
        pending: Vec<PendingImport>,
        should_continue: impl Fn() -> bool + Sync,
        emit: impl Fn(Vec<ImportResult>, Vec<RefreshedImportIdentity>) + Sync,
    ) {
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);
        let landed = std::sync::Mutex::new(Vec::<MeasuredImport>::new());
        drain_misses_owned(pending, |import| {
            if !should_continue() {
                return;
            }

            let result = self.analyze_and_cache(
                cache.as_ref(),
                context,
                &import.request,
                import.pending.key,
                import.pending.resolved,
                || true,
            );

            if !should_continue() {
                return;
            }

            let identity = RefreshedImportIdentity {
                specifier: import.detected.specifier,
                import_kind: import.detected.import_kind,
                named: import.detected.named,
                runtime: import.detected.runtime,
            };
            emit(vec![result.clone()], vec![identity.clone()]);
            landed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(MeasuredImport { result, identity });
        });

        // A superseded document gets no closing pass: the client already dropped its pushes.
        if !should_continue() {
            return;
        }
        let landed = landed
            .into_inner()
            .unwrap_or_else(|error| error.into_inner());
        let document = measured.into_iter().chain(landed).collect::<Vec<_>>();
        let (results, identities) = shared_bytes_corrections(document);
        if !results.is_empty() {
            emit(results, identities);
        }
    }

    fn analysis_items_for_detected(
        &self,
        context: &AnalysisContext,
        detected: Vec<DetectedImport>,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> Vec<ImportAnalysisItem> {
        // Cache hits and unresolvable imports settle at pool width; only a real miss queues for
        // an engine permit.
        let ready = |detected: &DetectedImport, request: ImportRequest, result: ImportResult| {
            ImportAnalysisItem {
                result: Some(result),
                detected: detected.clone(),
                status: ImportAnalysisStatus::Ready,
                message: None,
                request: Some(request),
            }
        };
        let mut items = drain_classified(
            &detected,
            |_, detected| {
                let request =
                    match import_request_for_detected(&context.active_document_path, detected) {
                        Ok(request) => request,
                        Err(message) => {
                            return Ok(ImportAnalysisItem {
                                detected: detected.clone(),
                                status: ImportAnalysisStatus::Missing,
                                message: Some(message),
                                request: None,
                                result: None,
                            });
                        }
                    };

                match self.probe_cache(context, &request, serve_stale, intent) {
                    CacheProbe::Hit(result) => Ok(ready(detected, request, *result)),
                    CacheProbe::Unresolved(message) => {
                        let result = analyze_unresolved_import(context, &request, message);
                        Ok(ready(detected, request, result))
                    }
                    CacheProbe::Miss(pending) => Err((request, pending)),
                }
            },
            |_, detected, (request, pending)| {
                let result = self.build_miss(context, &request, *pending);
                ready(detected, request, result)
            },
        );

        annotate_ready_items(&mut items);
        items
    }

    // L1 aggregate cache: return the cached FileSizeComputation when the file's
    // import set is unchanged (and node_modules has not been invalidated),
    // otherwise recompute once and overwrite this document's single slot.
    fn file_size_with_cache(
        &self,
        context: &AnalysisContext,
        active_document_path: &str,
        imports: &[SizedImport],
    ) -> crate::pipeline::file_size::FileSizeComputation {
        let cache = crate::pipeline::file_size_cache::shared_file_size_cache();
        let path = PathBuf::from(active_document_path);
        let signature = crate::pipeline::file_size_cache::file_size_signature(context, imports);

        if let Some(hit) = cache.get(&path, signature) {
            crate::logging::log_debug("file_size_cache", format!("hit: {}", path.display()));
            return hit;
        }

        crate::logging::log_debug("file_size_cache", format!("miss: {}", path.display()));
        let computed = compute_file_size(context, imports);
        // Offered unconditionally: `FileSizeCache::insert` itself refuses a floor or a degraded
        // total, so the gate cannot be forgotten at a call site.
        cache.insert(path, signature, computed.clone());
        computed
    }

    /// The lookup half of an analysis; `build_miss` is the build half.
    ///
    /// The engine permits bound builds, not cache hits, so hits are served at pool width and
    /// only misses enter the engine drain. A miss carries its resolved package and key so
    /// `build_miss` does not resolve the manifest again.
    fn probe_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> CacheProbe {
        let resolved = match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => resolved,
            Err(message) => return CacheProbe::Unresolved(message),
        };
        let key = cache_key_for_resolved_import(request, &resolved);
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);

        if serve_stale {
            let lookup_started_at = Instant::now();
            // SWR: serve the last-known value (flagged Stale/Unverified); the FileSizeDocument
            // handler revalidates a served Stale result in the background. The read promotes.
            if let Some((result, _freshness)) = cache.get_with_result_freshness(&key) {
                log_cache_lookup_timing(
                    request,
                    cache_read_mode_label(serve_stale, intent),
                    true,
                    Some(&result),
                    lookup_started_at.elapsed(),
                );
                return CacheProbe::Hit(Box::new(result));
            }
            log_cache_lookup_timing(
                request,
                cache_read_mode_label(serve_stale, intent),
                false,
                None,
                lookup_started_at.elapsed(),
            );
        } else {
            // Force-fresh (CI, §4.5): serve only a value verified `Fresh` against disk, from
            // memory or disk. `get_if_fresh` returns `None` on Unknown/Stale/Gone/miss, so a
            // transient `Unknown` never reaches CI as a `cache_hit`.
            if let Some(result) = fresh_cached_result_for_key(cache.as_ref(), request, &key, intent)
            {
                return CacheProbe::Hit(Box::new(result));
            }
        }

        CacheProbe::Miss(Box::new(PendingBuild { resolved, key }))
    }

    /// The build half. Only this may occupy an engine permit.
    fn build_miss(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        pending: PendingBuild,
    ) -> ImportResult {
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);
        self.analyze_and_cache(
            cache.as_ref(),
            context,
            request,
            pending.key,
            pending.resolved,
            || true,
        )
    }

    fn analyze_and_cache(
        &self,
        cache: &ImportCache,
        context: &AnalysisContext,
        request: &ImportRequest,
        key: String,
        resolved: ResolvedPackage,
        should_store: impl Fn() -> bool,
    ) -> ImportResult {
        let captured_generation = crate::cache::memory::cache_generation();
        let computed = self
            .analysis_flights
            .run_or_join(key.clone(), captured_generation, || {
                let (result, analyzed_graph) =
                    analyze_resolved_import_with_dependencies(context, request, resolved.clone());
                let dependency_fingerprints = if should_cache_result(&result) {
                    dependency_fingerprints(&resolved, analyzed_graph.as_ref())
                } else {
                    Vec::new()
                };
                let dependencies_are_reusable =
                    crate::cache::key::fingerprints_are_reusable(&dependency_fingerprints);

                ComputedAnalysis {
                    result,
                    dependency_fingerprints,
                    dependencies_are_reusable,
                }
            });

        if should_cache_result(&computed.result)
            && computed.dependencies_are_reusable
            && should_store()
        {
            cache.insert_with_fingerprints_at_generation(
                key,
                computed.result.clone(),
                computed.dependency_fingerprints.clone(),
                captured_generation,
            );
        }

        computed.result
    }
}

/// Test-only handle for seeding registry metadata; see
/// `ImportLensService::registry_hints_for_tests`.
pub struct RegistryHintTestHandle<'a> {
    service: &'a ImportLensService,
}

impl RegistryHintTestHandle<'_> {
    pub fn write_metadata_for_tests(
        &self,
        package_name: &str,
        latest_version: &str,
        fetched_at: u64,
    ) {
        let _ = self.service.registry_hints.write_metadata_for_tests(
            package_name,
            latest_version,
            fetched_at,
        );
    }
}

fn effective_registry_hint_mode(
    request: &AnalyzePackageJsonRequest,
) -> crate::registry::service::RegistryHintMode {
    match request.registry_hint_mode {
        Some(ProtocolRegistryHintMode::Off) => crate::registry::service::RegistryHintMode::Off,
        Some(ProtocolRegistryHintMode::Cached) => {
            crate::registry::service::RegistryHintMode::Cached
        }
        Some(ProtocolRegistryHintMode::RefreshStale) => {
            crate::registry::service::RegistryHintMode::RefreshStale
        }
        Some(ProtocolRegistryHintMode::ForceRefresh) => {
            crate::registry::service::RegistryHintMode::ForceRefresh
        }
        None if request.force_registry_refresh => {
            crate::registry::service::RegistryHintMode::ForceRefresh
        }
        None if request.include_registry_hints => {
            crate::registry::service::RegistryHintMode::Cached
        }
        None => crate::registry::service::RegistryHintMode::Off,
    }
}

/// Re-derive `shared_bytes` across a document's complete set of measurements and return only the
/// imports whose figure changed.
///
/// The client's figures were annotated against a partial set (cache hits only, or none for a
/// streamed import). `Some(0)` and `None` compare equal: the client gates on `> 0` in both
/// `insights.ts` and the tooltip, so re-sending would change nothing on screen.
fn shared_bytes_corrections(
    document: Vec<MeasuredImport>,
) -> (Vec<ImportResult>, Vec<RefreshedImportIdentity>) {
    let mut annotated = document
        .iter()
        .map(|import| import.result.clone())
        .collect::<Vec<_>>();
    annotate_shared_bytes(
        document
            .iter()
            .map(|import| import.identity.runtime)
            .zip(annotated.iter_mut()),
    );

    let mut results = Vec::new();
    let mut identities = Vec::new();
    for (import, result) in document.into_iter().zip(annotated) {
        if import.result.shared_bytes.unwrap_or_default() == result.shared_bytes.unwrap_or_default()
        {
            continue;
        }
        results.push(result);
        identities.push(import.identity);
    }

    (results, identities)
}

/// Shared-byte annotation across a document's *measured* imports.
///
/// Imports still being measured contribute nothing; `complete_pending_imports` re-derives the
/// figure once the last streamed import lands (`shared_bytes_corrections`).
fn annotate_ready_items(items: &mut [ImportAnalysisItem]) {
    annotate_shared_bytes(items.iter_mut().filter_map(|item| {
        // Read before the result is borrowed mutably.
        let runtime = item.detected.runtime;
        item.result.as_mut().map(|result| (runtime, result))
    }));
}

/// The version check and document parse both file-size document handlers share. The error arm is
/// boxed: it is a whole response, and it is the rare path.
fn file_size_document_prelude(
    request: &FileSizeDocumentRequest,
) -> Result<(AnalysisContext, Vec<DetectedImport>), Box<FileSizeDocumentResponse>> {
    if !(2..=PROTOCOL_VERSION).contains(&request.version) {
        return Err(Box::new(protocol_error_file_size_document_response(
            request,
            format!("unsupported protocol version {}", request.version),
        )));
    }

    let context = AnalysisContext {
        workspace_root: PathBuf::from(&request.workspace_root),
        active_document_path: PathBuf::from(&request.active_document_path),
    };
    let ignore_resolver = IgnoreRuleResolver::default();
    let detected = detected_imports_for_document(
        &request.active_document_path,
        &request.source,
        true,
        &ignore_resolver,
    )
    .map_err(|error| {
        Box::new(FileSizeDocumentResponse {
            version: request.version,
            request_id: request.request_id,
            raw_bytes: 0,
            minified_bytes: 0,
            gzip_bytes: 0,
            brotli_bytes: 0,
            zstd_bytes: 0,
            imports: Vec::new(),
            states: Vec::new(),
            asset_breakdown: Vec::new(),
            // Nothing was summed; clients refuse an errored response.
            incomplete: false,
            degraded: false,
            error: Some(error.clone()),
            diagnostics: vec![ImportDiagnostic::for_stage("document_parse", &error)],
        })
    })?;

    Ok((context, detected))
}

/// Whether a result may be written to the import cache (ADR-0006, invariant 3).
///
/// A pre-check, not the gate: the gate is `ImportResult::is_durable` inside the stores
/// (`ImportCache::insert*`, `DiskCache::insert*`). This asks the same question only to skip
/// computing fingerprints for an insert that would be refused.
///
/// Cached: a Measured result, and an Unmeasured one whose stage is a property of the package's
/// bytes (`parse`, `link`, `oversized_entry`, an unreadable manifest, an unresolvable entry). The
/// key's fingerprints expire such a fact exactly when it would change; refusing it would rebuild
/// a broken package on every analysis.
///
/// Not cached: a transient outcome, an IO condition (`entry_metadata`), and any unclassified
/// stage. See `pipeline::stage::may_enter_a_durable_store`.
fn should_cache_result(result: &ImportResult) -> bool {
    result.is_durable()
}

fn cache_read_mode_label(serve_stale: bool, intent: ReadIntent) -> &'static str {
    match (serve_stale, intent) {
        (true, ReadIntent::Interactive) => "serve_stale_interactive",
        (true, ReadIntent::Bulk) => "serve_stale_bulk",
        (false, ReadIntent::Interactive) => "force_fresh_interactive",
        (false, ReadIntent::Bulk) => "force_fresh_bulk",
    }
}

fn fresh_cached_result_for_resolved_import(
    cache: &ImportCache,
    request: &ImportRequest,
    resolved: &ResolvedPackage,
    intent: ReadIntent,
) -> (String, Option<ImportResult>) {
    let key = cache_key_for_resolved_import(request, resolved);
    let result = fresh_cached_result_for_key(cache, request, &key, intent);
    (key, result)
}

fn fresh_cached_result_for_key(
    cache: &ImportCache,
    request: &ImportRequest,
    key: &str,
    intent: ReadIntent,
) -> Option<ImportResult> {
    let lookup_started_at = Instant::now();
    let result = match intent {
        ReadIntent::Interactive => cache.get_if_fresh_and_promote(key),
        ReadIntent::Bulk => cache.get_if_fresh(key),
    };
    log_cache_lookup_timing(
        request,
        cache_read_mode_label(false, intent),
        result.is_some(),
        result.as_ref(),
        lookup_started_at.elapsed(),
    );
    result
}

fn log_cache_lookup_timing(
    request: &ImportRequest,
    mode: &str,
    hit: bool,
    result: Option<&ImportResult>,
    elapsed: Duration,
) {
    if elapsed < SLOW_CACHE_LOOKUP_LOG_THRESHOLD {
        return;
    }

    let freshness = result
        .map(|result| format!("{:?}", result.freshness.kind))
        .unwrap_or_else(|| "miss".to_owned());
    crate::logging::log_debug(
        "cache",
        format!(
            "slow cache lookup for package={} specifier={} mode={} hit={} freshness={} elapsed={}ms",
            request.package_name.as_str(),
            request.specifier.as_str(),
            mode,
            hit,
            freshness,
            elapsed.as_millis()
        ),
    );
}

fn revalidation_claim_key(
    cache_key: &str,
    workspace_root: &str,
    document_path: &str,
    generation: Option<u64>,
) -> String {
    format!(
        "{cache_key}\0{workspace_root}\0{document_path}\0{}",
        generation
            .map(|value| value.to_string())
            .unwrap_or_default()
    )
}

#[cfg(test)]
#[path = "../tests/unit/service_swr.rs"]
mod service_swr_tests;

#[cfg(test)]
#[path = "../tests/unit/service_registry_budget.rs"]
mod service_registry_budget_tests;

fn detected_imports_for_document(
    active_document_path: &str,
    source: &str,
    apply_ignore_rules: bool,
    ignore_resolver: &IgnoreRuleResolver,
) -> Result<Vec<DetectedImport>, String> {
    let mut imports = analyze_imports(active_document_path, source)?;

    if apply_ignore_rules {
        let active_path = Path::new(active_document_path);
        let rules = ignore_resolver.rules_for(active_path);
        imports.retain(|detected| !should_ignore_import(detected, active_document_path, &rules));
    }

    Ok(imports)
}

fn detected_import_for_specifier(specifier: &str) -> DetectedImport {
    DetectedImport {
        specifier: specifier.to_owned(),
        package_name: get_package_name(specifier),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        syntax: ImportSyntax::Static,
        runtime: ImportRuntime::Component,
        line: 0,
        quote_end: Default::default(),
        specifier_range: Default::default(),
        statement_range: Default::default(),
    }
}

fn import_request_for_detected(
    active_document_path: &Path,
    detected: &DetectedImport,
) -> Result<ImportRequest, String> {
    let version = resolve_installed_package_version(active_document_path, &detected.package_name)?;

    Ok(ImportRequest {
        specifier: detected.specifier.clone(),
        package_name: detected.package_name.clone(),
        version,
        named: detected.named.clone(),
        import_kind: detected.import_kind,
        // The one derivation carrying the runtime split onto the request `pipeline::file_size`
        // groups builds by. A constant here collapses an Astro file's Server and Client imports
        // into one bundle and under-reports the total (ADR-0005). Pinned by
        // `tests/file_size_runtime.rs`.
        runtime: detected.runtime,
    })
}

fn resolve_installed_package_version(
    active_document_path: &Path,
    package_name: &str,
) -> Result<String, String> {
    let package_root = find_package_root(active_document_path, package_name)
        .map_err(|_| "Package not found".to_owned())?;
    let package_json_path = package_root.join("package.json");
    let contents =
        fs::read_to_string(&package_json_path).map_err(|_| "Package not found".to_owned())?;
    let Ok(json) = serde_json::from_str::<Value>(&contents) else {
        return Ok("unknown".to_owned());
    };

    Ok(json
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned())
}

fn package_name_from_package_json_path(package_json_path: &str) -> Option<String> {
    let normalized = package_json_path.replace('\\', "/");
    let marker = "/node_modules/";
    let index = normalized.rfind(marker)?;
    let after_node_modules = normalized[index + marker.len()..]
        .strip_suffix("/package.json")
        .unwrap_or(&normalized[index + marker.len()..]);

    Some(get_package_name(after_node_modules))
}

/// Fingerprint the paths used by the successful engine result, or the
/// conservative manifest+entry pair used by static fallback.
fn dependency_fingerprints(
    resolved: &ResolvedPackage,
    source: Option<&crate::pipeline::analyze::FingerprintSource>,
) -> Vec<crate::cache::key::FileFingerprint> {
    use crate::cache::key::{file_fingerprint_reading_hash, sort_and_dedup_fingerprints};
    use crate::pipeline::analyze::FingerprintSource;

    let mut fingerprints = match source {
        // The engine fingerprinted each module as it read it, so the hash describes the exact
        // bytes measured. Only the manifest and unread binary modules are hashed here.
        Some(FingerprintSource::ReadTime {
            fingerprints,
            stat_paths,
        }) => {
            let mut all = fingerprints.clone();
            all.extend(
                stat_paths
                    .iter()
                    .cloned()
                    .filter_map(file_fingerprint_reading_hash),
            );
            all
        }
        // Static fallback: no graph was built, so nothing measured can disagree with these.
        None => vec![
            resolved.package_root.join("package.json"),
            resolved.entry_path.clone(),
        ]
        .into_iter()
        .filter_map(file_fingerprint_reading_hash)
        .collect(),
    };

    // Two ids can canonicalize to the same real path (a symlinked workspace dep).
    sort_and_dedup_fingerprints(&mut fingerprints);
    fingerprints
}
/// A protocol-error analyze response. Lives here because the streaming document handler builds
/// one itself.
pub fn protocol_error_analyze_document_response(
    request: &AnalyzeDocumentRequest,
    message: String,
) -> AnalyzeDocumentResponse {
    AnalyzeDocumentResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        imports: Vec::new(),
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic::for_stage("protocol", message)],
    }
}

pub fn protocol_error_file_size_document_response(
    request: &FileSizeDocumentRequest,
    message: String,
) -> FileSizeDocumentResponse {
    FileSizeDocumentResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        imports: Vec::new(),
        states: Vec::new(),
        asset_breakdown: Vec::new(),
        incomplete: false,
        degraded: false,
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic::for_stage("protocol", message)],
    }
}

/// The runtime a direct `enumerate_exports` request resolves under, from the document classifier
/// (`document::runtime_at_offset`) so it agrees with the size path.
///
/// The daemon classifies (ADR-0002) from the document on disk at the cursor's UTF-16 offset.
/// With no offset or an unreadable file the answer is `Component`, the default for a document
/// with no runtime-bearing regions.
fn runtime_for_enumeration(request: &EnumerateExportsRequest) -> ImportRuntime {
    let Some(offset) = request.cursor_offset else {
        return ImportRuntime::Component;
    };

    match fs::read_to_string(&request.active_document_path) {
        Ok(source) => runtime_at_offset(&request.active_document_path, &source, offset),
        Err(_) => ImportRuntime::Component,
    }
}

pub fn protocol_error_exports_response(
    request: &EnumerateExportsRequest,
    message: String,
) -> EnumerateExportsResponse {
    EnumerateExportsResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        specifier: request.specifier.clone(),
        exports: Vec::new(),
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: "protocol".to_owned(),
            message,
            details: vec![format!("specifier: {}", request.specifier)],
        }],
    }
}

#[cfg(test)]
mod report_panic_isolation_tests {
    use super::ImportLensService;
    use crate::document::IgnoreRuleResolver;
    use crate::ipc::protocol::{PROTOCOL_VERSION, WorkspaceReportBudgets, WorkspaceReportRequest};
    use std::path::Path;

    #[test]
    fn analyze_report_source_isolates_a_panicking_file() {
        let service = ImportLensService::new(None, false);
        let request = WorkspaceReportRequest {
            message_type: "workspace_report".to_owned(),
            version: PROTOCOL_VERSION,
            request_id: 1,
            workspace_root: "unused".to_owned(),
            budgets: WorkspaceReportBudgets {
                per_import_brotli_bytes: None,
            },
        };

        // The cfg(test) sentinel makes per-file analysis panic; the file is skipped, not fatal.
        let items = service.analyze_report_source(
            Path::new("bad.ts"),
            &request,
            "// __IMPORTLENS_FORCE_PANIC__\n".to_owned(),
            &IgnoreRuleResolver::default(),
        );

        assert!(items.is_empty());
    }
}

#[cfg(test)]
mod task_lifecycle_tests {
    use super::{ImportLensService, should_rearm_revalidation, try_begin_cache_maintenance};
    use crate::cache::key::Freshness;
    use crate::ipc::protocol::{PROTOCOL_VERSION, WorkspaceReportBudgets, WorkspaceReportRequest};

    // The mid-recompute race is not deterministically reproducible, so the re-arm decision is
    // tested directly: only `Stale` re-arms.
    #[test]
    fn swr_re_arms_one_trailing_revalidation_only_when_still_stale() {
        assert!(
            should_rearm_revalidation(Some(Freshness::Stale)),
            "a still-Stale entry re-arms one trailing revalidation"
        );
        assert!(!should_rearm_revalidation(Some(Freshness::Fresh)));
        assert!(
            !should_rearm_revalidation(Some(Freshness::Unknown)),
            "a graduated transient Unknown must never route into recompute"
        );
        assert!(!should_rearm_revalidation(Some(Freshness::Gone)));
        assert!(!should_rearm_revalidation(None));
    }

    // The overlapping-pass race is not deterministically reproducible, so the claim is tested
    // directly: exclusive while held, freed on drop.
    #[test]
    fn maintenance_skips_when_already_running() {
        let guard = try_begin_cache_maintenance().expect("first claim should win");
        assert!(
            try_begin_cache_maintenance().is_none(),
            "a second maintenance pass is a no-op while one is in flight"
        );
        drop(guard);
        let next = try_begin_cache_maintenance().expect("claim frees on drop for the next pass");
        drop(next);
    }

    // An aggregation panic surfaces as an error response, not a dropped `oneshot` sender.
    #[test]
    fn workspace_report_aggregation_panic_yields_error_response() {
        let service = std::sync::Arc::new(ImportLensService::new(None, false));
        let (tx, rx) = tokio::sync::oneshot::channel();
        service.spawn_workspace_report(
            WorkspaceReportRequest {
                message_type: "workspace_report".to_owned(),
                version: PROTOCOL_VERSION,
                request_id: 77,
                // cfg(test) sentinel: panics inside the aggregation.
                workspace_root: "__IMPORTLENS_FORCE_REPORT_PANIC__".to_owned(),
                budgets: WorkspaceReportBudgets {
                    per_import_brotli_bytes: None,
                },
            },
            tx,
        );
        let response = rx
            .blocking_recv()
            .expect("catch_unwind must send an error response, not drop the sender");
        assert_eq!(response.request_id, 77);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|message| message.contains("panicked")),
            "an aggregation panic must yield an error response: {response:?}"
        );
        assert!(response.rows.is_empty());
    }
}

#[cfg(test)]
mod analyze_and_cache_single_flight_tests {
    use super::{ComputedAnalysis, ImportLensService};
    use crate::cache::key::cache_key_for_resolved_import;
    use crate::ipc::protocol::{
        ConfidenceLevel, ImportKind, ImportRequest, ImportResult, ImportRuntime, MeasuredSizes,
    };
    use crate::pipeline::analyze::AnalysisContext;
    use crate::pipeline::resolver::{ResolvedPackage, SideEffectsMode};
    use std::{
        sync::{Arc, Condvar, Mutex, mpsc},
        thread,
        time::Duration,
    };

    fn cacheable_result(specifier: &str) -> ImportResult {
        let mut result = ImportResult::measured(
            specifier,
            MeasuredSizes {
                raw_bytes: 42,
                minified_bytes: 21,
                gzip_bytes: 10,
                brotli_bytes: 8,
                zstd_bytes: 9,
            },
        );
        result.side_effects = true;
        result.confidence = ConfidenceLevel::High;
        result
    }

    fn wait_until_released(pair: &(Mutex<bool>, Condvar)) {
        let (lock, cvar) = pair;
        let mut released = lock.lock().expect("release lock");
        while !*released {
            released = cvar.wait(released).expect("release wait");
        }
    }

    fn release(pair: &(Mutex<bool>, Condvar)) {
        let (lock, cvar) = pair;
        *lock.lock().expect("release lock") = true;
        cvar.notify_all();
    }

    fn request() -> ImportRequest {
        ImportRequest {
            specifier: "pkg-flight".to_owned(),
            package_name: "pkg-flight".to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Dynamic,
            runtime: ImportRuntime::Component,
        }
    }

    fn resolved(workspace: &std::path::Path) -> ResolvedPackage {
        let package_root = workspace.join("node_modules").join("pkg-flight");
        ResolvedPackage {
            package_root: package_root.clone(),
            package_json: serde_json::json!({ "name": "pkg-flight", "version": "1.0.0" }),
            entry_path: package_root.join("index.js"),
            is_cjs: false,
            side_effects: SideEffectsMode::True,
        }
    }

    #[test]
    fn analyze_and_cache_follower_keeps_own_cache_write_when_leader_does_not_store() {
        // The follower re-reads the process-global generation; a sibling test bumping it would
        // make the follower its own leader.
        let _generation = crate::cache::memory::hold_cache_generation_steady();
        let service = Arc::new(ImportLensService::new(None, false));
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let workspace = std::env::temp_dir().join(format!(
            "il-analysis-flight-cache-write-{}-{unique}",
            std::process::id()
        ));
        let context = AnalysisContext {
            workspace_root: workspace.clone(),
            active_document_path: workspace.join("src").join("app.ts"),
        };
        let request = request();
        let resolved = resolved(&workspace);
        let key = cache_key_for_resolved_import(&request, &resolved);
        let cache = service
            .cache_registry
            .cache_for_root(&context.workspace_root);
        let generation = crate::cache::memory::cache_generation();
        let release_compute = Arc::new((Mutex::new(false), Condvar::new()));
        let (leader_started_tx, leader_started_rx) = mpsc::channel();

        let leader_service = Arc::clone(&service);
        let leader_key = key.clone();
        let leader_release = Arc::clone(&release_compute);
        let leader = thread::spawn(move || {
            leader_service
                .analysis_flights
                .run_or_join(leader_key, generation, || {
                    leader_started_tx.send(()).expect("leader started");
                    wait_until_released(&leader_release);
                    ComputedAnalysis {
                        result: cacheable_result("pkg-flight"),
                        dependency_fingerprints: Vec::new(),
                        dependencies_are_reusable: true,
                    }
                })
        });

        leader_started_rx.recv().expect("leader should start");

        let follower_service = Arc::clone(&service);
        let follower_cache = Arc::clone(&cache);
        let follower_context = context.clone();
        let follower_request = request.clone();
        let follower_key = key.clone();
        let follower_resolved = resolved.clone();
        let follower = thread::spawn(move || {
            follower_service.analyze_and_cache(
                follower_cache.as_ref(),
                &follower_context,
                &follower_request,
                follower_key,
                follower_resolved,
                || true,
            )
        });

        thread::sleep(Duration::from_millis(50));
        release(&release_compute);

        let leader_result = leader.join().expect("leader thread");
        let follower_result = follower.join().expect("follower thread");

        assert_eq!(leader_result.result, cacheable_result("pkg-flight"));
        assert_eq!(follower_result, cacheable_result("pkg-flight"));
        assert!(
            cache.get_for_prewarm(&key).is_some(),
            "a follower with should_store=true must keep its cache write even when the leader did not store",
        );
    }
}

/// Property over every durable store the daemon writes, quantified over every stage that is not a
/// property of the package's bytes. It hands each store a real result and asks what it kept, so
/// it tests the stores' own gates, not a predicate a caller might forget.
///
/// The build-derived stores (`pipeline::full_package`, `pipeline::export_list`,
/// `pipeline::build_memo`, `engine::dependency_paths`) are absent on purpose: their only inputs
/// exist solely on the `Ok` side of a build, so a failure is unrepresentable there.
/// `scripts/test/result-model-guards.test.mjs` fails if a result is plumbed into one.
///
/// The extension's persisted histories are covered in `extension/test/analysis/transience.test.ts`.
#[cfg(test)]
mod every_durable_store_rejects_a_non_durable_outcome {
    use super::should_cache_result;
    use crate::cache::disk::DiskCache;
    use crate::cache::key::FileFingerprint;
    use crate::cache::memory::{CachedImport, ImportCache};
    use crate::engine::stage;
    use crate::ipc::protocol::{
        ImportDiagnostic, ImportKind, ImportRequest, ImportResult, ImportRuntime, MeasuredSizes,
    };
    use crate::pipeline::file_size::{
        FileSizeComputation, SizedImport, per_import_totals_for_test,
    };
    use crate::pipeline::file_size_cache::FileSizeCache;
    use crate::pipeline::stage as pipeline_stage;
    use std::path::PathBuf;

    /// Every stage a durable store must refuse: request-local engine outcomes plus machine-local
    /// pipeline work. Derived from the allowlist, so a reclassified stage moves with it.
    fn non_durable_stages() -> Vec<&'static str> {
        stage::ALL
            .iter()
            .chain(pipeline_stage::ALL.iter())
            .copied()
            .filter(|candidate| !pipeline_stage::may_enter_a_durable_store(candidate))
            .collect()
    }

    /// The engine failure stages that ARE a property of the package's bytes.
    fn durable_failure_stages() -> Vec<&'static str> {
        stage::ALL
            .iter()
            .copied()
            .filter(|candidate| pipeline_stage::may_enter_a_durable_store(candidate))
            .collect()
    }

    fn measured(specifier: &str, bytes: u64) -> ImportResult {
        ImportResult::measured(
            specifier,
            MeasuredSizes {
                raw_bytes: bytes,
                minified_bytes: bytes,
                gzip_bytes: bytes,
                brotli_bytes: bytes,
                zstd_bytes: bytes,
            },
        )
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

    /// The L2 envelope around a result, exactly as `ImportCache` builds one.
    fn cached(result: ImportResult) -> CachedImport {
        use std::sync::{Arc, atomic::AtomicU64};

        CachedImport {
            result,
            dependency_fingerprints: Vec::new(),
            verified_generation: 0,
            verified_at: None,
            first_party: false,
            last_seq: Arc::new(AtomicU64::new(1)),
            persisted_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The L1 file-size aggregate, built from per-import results the way the real fallback builds
    /// it: ADR-0006 invariant 4 concerns how a result is turned into a total.
    fn file_total(results: Vec<(&str, ImportResult)>) -> FileSizeComputation {
        let sized = results
            .into_iter()
            .map(|(specifier, result)| SizedImport::installed(request(specifier), Some(result)))
            .collect::<Vec<_>>();
        per_import_totals_for_test(&sized)
    }

    /// The L1 import cache store itself.
    #[test]
    fn the_l1_import_cache_refuses_a_non_durable_result() {
        for stage in non_durable_stages() {
            let cache = ImportCache::new(None, false);
            let key = format!("v4:healthy-lib:{stage}");
            let result =
                ImportResult::unmeasured("healthy-lib", stage, "build did not finish", vec![]);

            assert!(
                result.sizes().is_none(),
                "`{stage}`: the premise — there is no size to store in the first place"
            );
            assert!(!should_cache_result(&result), "`{stage}`");

            cache.insert(key.clone(), result);
            assert!(
                cache.get_for_prewarm(&key).is_none(),
                "`{stage}` says nothing about the package's bytes; the L1 store must keep nothing"
            );
        }
    }

    /// The L1 import cache, other transient shape: a successful build whose full-package
    /// comparison build failed transiently. The sizes are real but `truly_treeshakeable: false` is
    /// an accident; caching it would mark a healthy package "not tree-shakeable".
    #[test]
    fn the_l1_import_cache_refuses_a_measurement_whose_comparison_build_degraded_transiently() {
        for stage in stage::ALL
            .iter()
            .copied()
            .filter(|candidate| pipeline_stage::is_transient(candidate))
        {
            let cache = ImportCache::new(None, false);
            let key = format!("v4:healthy-lib:comparison:{stage}");
            let mut result = measured("healthy-lib", 17_550);
            result.diagnostics.push(ImportDiagnostic::for_stage(
                stage,
                "full-package comparison build failed; treating as not tree-shakeable",
            ));

            assert!(
                result.sizes().is_some(),
                "`{stage}`: the premise — this one really was measured"
            );
            assert!(!should_cache_result(&result), "`{stage}`");

            cache.insert(key.clone(), result);
            assert!(
                cache.get_for_prewarm(&key).is_none(),
                "`{stage}`: real sizes, but a tree-shaking verdict that is a scheduling accident"
            );
        }
    }

    /// The L2 disk cache: it outlives the process, and is gated independently of the L1 in front.
    #[test]
    fn the_l2_disk_cache_refuses_a_non_durable_result() {
        let dir = std::env::temp_dir().join(format!(
            "il-durable-l2-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = DiskCache::new(Some(dir.clone()), true);

        for stage in non_durable_stages() {
            let key = format!("v4:healthy-lib:{stage}");
            let entry = cached(ImportResult::unmeasured(
                "healthy-lib",
                stage,
                "build did not finish",
                vec![],
            ));

            disk.insert(&key, &entry)
                .expect("a refusal is a no-op, never an Err — an Err would mark the key dirty");
            disk.flush_pending_inserts();

            assert!(
                disk.get_with_freshness(&key)
                    .map(|(cached, _)| cached)
                    .is_none(),
                "`{stage}`: L2 outlives the process; a scheduling accident must not"
            );
        }

        // Control: a deterministic failure is persisted; it expires with the package's bytes.
        let entry = cached(ImportResult::unmeasured(
            "broken-lib",
            stage::PARSE,
            "unexpected token",
            vec![],
        ));
        disk.insert("v4:broken-lib:parse", &entry)
            .expect("enqueue a deterministic failure");
        disk.flush_pending_inserts();
        assert!(
            disk.get_with_freshness("v4:broken-lib:parse")
                .map(|(cached, _)| cached)
                .is_some(),
            "a deterministic failure is a fact about the package and IS persisted (invariant 3)"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The L1 file-size aggregate, over every non-durable stage, every durable one, and an import
    /// still being measured: invariant 4 is about the total's inputs, not about failure.
    #[test]
    fn the_l1_file_size_cache_refuses_a_floor() {
        let path = PathBuf::from("C:/ws/src/index.ts");

        for stage in non_durable_stages() {
            let cache = FileSizeCache::new();
            let total = file_total(vec![
                ("alpha", measured("alpha", 100)),
                (
                    "beta",
                    ImportResult::unmeasured("beta", stage, "no", vec![]),
                ),
            ]);

            assert!(
                total.incomplete,
                "`{stage}`: an import contributed no bytes"
            );
            cache.insert(path.clone(), 1, total);
            assert!(
                cache.get(&path, 1).is_none(),
                "`{stage}`: a floor served as the file's size for the whole 30s TTL"
            );
        }

        // A deterministic failure is cached per import (invariant 3) and still makes the file's
        // total a floor (invariant 4).
        for stage in durable_failure_stages() {
            let cache = FileSizeCache::new();
            let result = ImportResult::unmeasured("beta", stage, "no matching export", vec![]);
            assert!(
                should_cache_result(&result),
                "`{stage}`: the per-import failure IS cached — it is a fact about the bytes"
            );

            let total = file_total(vec![("alpha", measured("alpha", 100)), ("beta", result)]);
            assert!(
                total.incomplete,
                "`{stage}`: beta contributed no bytes, so the file's total is a FLOOR"
            );
            cache.insert(path.clone(), 1, total);
            assert!(
                cache.get(&path, 1).is_none(),
                "`{stage}`: deterministically unknown is still unknown, and a floor is never cached"
            );
        }

        // An import whose own build has not landed yet.
        let cache = FileSizeCache::new();
        let loading = per_import_totals_for_test(&[
            SizedImport::installed(request("alpha"), Some(measured("alpha", 100))),
            SizedImport::installed(request("beta"), None),
        ]);
        assert!(loading.incomplete);
        cache.insert(path.clone(), 1, loading);
        assert!(cache.get(&path, 1).is_none());

        // What `incomplete` cannot see (ADR-0006, invariant 4): every contributor measured but the
        // file's own combined build failed, so the number is an un-deduplicated per-import sum.
        // `file_size.rs` proves the flag is raised; this proves the store refuses it.
        let cache = FileSizeCache::new();
        let mut over_counted = file_total(vec![
            ("alpha", measured("alpha", 100)),
            ("beta", measured("beta", 20)),
        ]);
        over_counted.degraded = true;
        assert!(!over_counted.incomplete && over_counted.error.is_none());
        cache.insert(path.clone(), 1, over_counted);
        assert!(
            cache.get(&path, 1).is_none(),
            "a degraded total is an OVER-count of the file, and just as unusable as a floor"
        );

        // Control: every import measured, so the total caches.
        let cache = FileSizeCache::new();
        let complete = file_total(vec![
            ("alpha", measured("alpha", 100)),
            ("beta", measured("beta", 20)),
        ]);
        assert!(!complete.incomplete);
        cache.insert(path.clone(), 1, complete);
        assert!(
            cache.get(&path, 1).is_some(),
            "a total whose every input was measured is the file's size, and is cached"
        );
    }

    /// The other half: a deterministic per-import outcome is cached, sizes or not. It is keyed by
    /// the package bytes' fingerprints, and refusing it would rebuild a broken package on every
    /// analysis.
    #[test]
    fn the_l1_import_cache_still_keeps_every_deterministic_outcome() {
        for stage in durable_failure_stages() {
            let cache = ImportCache::new(None, false);
            let key = format!("v4:broken-lib:{stage}");
            let result =
                ImportResult::unmeasured("broken-lib", stage, "no matching export", vec![]);

            cache.insert_with_fingerprints(key.clone(), result, Vec::<FileFingerprint>::new());
            assert!(
                cache.get_for_prewarm(&key).is_some(),
                "`{stage}` will happen again next time; withholding it buys a rebuild and no \
                 correctness"
            );
        }
    }
}
