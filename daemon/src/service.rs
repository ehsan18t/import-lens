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
        package_json_dependency_sections, should_ignore_import,
    },
    ipc::protocol::{
        AnalyzeDocumentRequest, AnalyzeDocumentResponse, AnalyzePackageJsonRequest,
        AnalyzePackageJsonResponse, AnalyzeSpecifiersRequest, AnalyzeSpecifiersResponse,
        BatchRequest, BatchResponse, CacheListRequest, CacheListResponse, CacheRemoveRequest,
        CacheRemoveResponse, CacheRemoveScope, CacheStatusRequest, CacheStatusResponse,
        CompleteImportMembersRequest, CompleteImportMembersResponse, ConfidenceLevel,
        DetectedImport, EnumerateExportsRequest, EnumerateExportsResponse, FileSizeDocumentRequest,
        FileSizeDocumentResponse, FileSizeRequest, FileSizeResponse, FreshnessKind,
        ImportAnalysisItem, ImportAnalysisStatus, ImportDiagnostic, ImportKind, ImportRequest,
        ImportResult, ImportRuntime, ImportSyntax, PROTOCOL_VERSION,
        PackageJsonDependencyAnalysisItem, RefreshedImportIdentity,
        RegistryHintMode as ProtocolRegistryHintMode, RegistryHintResult, RegistryHintTarget,
        WorkspaceReportRequest, WorkspaceReportResponse, WorkspaceReportSummary,
        is_supported_protocol_version,
    },
    pipeline::analyze::{AnalysisContext, analyze_import, analyze_resolved_import_with_graph},
    pipeline::file_size::{annotate_shared_bytes, compute_file_size},
    pipeline::graph::{
        ModuleGraph, build_module_graph_cached, clear_module_graph_cache,
        invalidate_module_graph_cache_for_package, module_exported_names,
    },
    pipeline::resolver::{ResolvedPackage, find_package_root, resolve_package_entry},
};
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Whether a cached-analyze read promotes the entry's LRU recency (scan
/// resistance, §5.1). An interactive read whose result the user is looking at now
/// promotes; a bulk/background scan (`WorkspaceReport`, `Compare`) does not, so a
/// full-workspace pass can't flood the recency signal and evict the user's warm
/// working set. When a caller's intent is ambiguous, prefer `Interactive` —
/// over-promoting is safe; the finding targets full-workspace report/Compare scans.
#[derive(Clone, Copy)]
enum ReadIntent {
    Interactive,
    Bulk,
}

const SLOW_CACHE_LOOKUP_LOG_THRESHOLD: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct ComputedAnalysis {
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
}

/// F1 trailing-re-check decision for the background SWR revalidation. After a
/// revalidation recomputes and re-inserts a key, its freshness is re-probed: a
/// `Stale` result means a dependency changed AGAIN while the recompute ran (and a
/// concurrent stale serve was coalesced away by the in-flight guard), so exactly
/// ONE more revalidation is re-armed and the served value catches up to the newer
/// state without waiting for the next interactive read. A graduated transient
/// `Unknown` is NEVER re-armed — re-analyzing would re-hit the same stat/read
/// error and could overwrite the good cached value; `Fresh`/`Gone`/absent need no
/// re-run either.
fn should_rearm_revalidation(freshness: Option<crate::cache::key::Freshness>) -> bool {
    matches!(freshness, Some(crate::cache::key::Freshness::Stale))
}

/// Process-global "a cache-maintenance pass is running" flag (F4-B). Cache
/// maintenance operates on the process-global on-disk cache — a single storage
/// path shared by every service instance — so passes must serialize even across
/// instances: a re-Hello builds a fresh service and spawns a new maintenance task
/// whose immediate first tick would otherwise run concurrently with the previous
/// connection's still-detached `spawn_blocking` pass. The passes already serialize
/// on redb's single writer; this simply skips the redundant duplicate scan.
static CACHE_MAINTENANCE_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// RAII claim on [`CACHE_MAINTENANCE_IN_PROGRESS`]. Clears the flag on drop —
/// including on panic/unwind — so a panicking pass cannot wedge maintenance off.
struct MaintenanceGuard;

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        CACHE_MAINTENANCE_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Claims the maintenance-in-progress flag. Returns `Some(guard)` for the caller
/// that wins the claim (which should run the pass) and `None` while a pass is
/// already in flight, so a redundant concurrent pass is skipped. The guard
/// releases the claim on drop.
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

// `RegistryHintService` and `RegistryRefreshExecutor` hold trait objects and a
// thread pool respectively, so `ImportLensService` no longer derives `Debug`.
pub struct ImportLensService {
    cache_registry: ProjectCacheRegistry,
    analysis_flights: AnalysisFlightRegistry<ComputedAnalysis>,
    registry_hints: crate::registry::service::RegistryHintService,
    registry_executor: crate::registry::executor::RegistryRefreshExecutor,
    report_executor: crate::report::executor::WorkspaceReportExecutor,
    // Registry-metadata store byte budget (`importLens.registryCacheMaxSizeMB`,
    // wired through Hello — RB-16). The maintenance pass caps the registry store at
    // this instead of the hardcoded default constant.
    registry_cache_max_size_bytes: u64,
    // Set only by `new_with_registry_hints_for_tests`. When true, the IPC
    // server's Hello handler preserves `registry_hints`/`registry_executor`
    // across the Hello-driven service rebuild instead of reconstructing them
    // from `hello.storage_path` with the real `UreqRegistryHttpClient`. This
    // lets integration tests inject a fake `RegistryHttpClient` (e.g. to
    // control fetch timing/failure deterministically) that survives the
    // handshake. See `daemon/src/ipc/server.rs`'s `Hello` handling.
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

    /// Test-only: exists so integration tests (`daemon/tests/*.rs`, which
    /// compile the daemon lib as an external crate and therefore cannot see
    /// `#[cfg(test)]` items) can inject a fake `RegistryHintService`. See the
    /// `preserve_registry_across_hello` field doc comment for why the IPC
    /// server must special-case services constructed this way.
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

    /// Test-only: exists so integration tests can seed cached registry
    /// metadata without a real network fetch. See
    /// `new_with_registry_hints_for_tests` for why this cannot be
    /// `#[cfg(test)]`-gated.
    pub fn registry_hints_for_tests(&self) -> RegistryHintTestHandle<'_> {
        RegistryHintTestHandle { service: self }
    }

    /// Rebuilds only the cache-registry portion of the service for a freshly
    /// negotiated Hello handshake while preserving the existing
    /// `registry_hints`/`registry_executor`. Only called by the IPC server
    /// when `preserve_registry_across_hello()` is true (i.e. the service was
    /// constructed via `new_with_registry_hints_for_tests`); production
    /// connections always rebuild via `new_with_cache_policy` so that
    /// `hello.storage_path` remains the source of truth for registry
    /// configuration.
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

    /// Startup recency seed (C5 / Finding 10d, §3.3). Delegated to the cache
    /// registry; the IPC server calls this synchronously in its Hello handler —
    /// AFTER the registry is (re)built with the negotiated disk config and BEFORE
    /// any analyze/cache request is served — so no new entry is created with a
    /// pre-seed low seq. See `ProjectCacheRegistry::seed_recency_clock_from_disk`.
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

    /// Fans a bulk "refresh dependency block" onto the isolated registry pool
    /// (D7 / §6.1). Three properties this drain guarantees:
    ///
    /// * **Cache first.** Non-cancelled targets are classified through one
    ///   cache-only pre-pass. Cache-eligible results stream immediately, and
    ///   only unresolved targets are enqueued for network refresh.
    ///
    /// * **Bounded in flight.** Each target is a `spawn` onto the
    ///   `REGISTRY_REFRESH_CONCURRENCY`-thread pool, never a per-target thread,
    ///   so at most the pool size is ever fetching at once — the pool IS the
    ///   in-flight cap; no extra semaphore is layered on.
    /// * **Cancellable.** Every job re-reads the shared `cancelled` flag BEFORE
    ///   its network fetch. Once a newer block supersedes this one (or the
    ///   connection ends) the flag flips and each still-queued job skips its
    ///   fetch and reports `None` — no error is surfaced for the skipped work,
    ///   and the jobs already in flight are left to finish. The registry is
    ///   blocking std on rayon, so a plain `Arc<AtomicBool>` checked per job is
    ///   the fit, not a tokio token.
    ///
    /// `on_result` is invoked exactly once per target with the job's index and
    /// either the fetched `RegistryHintResult` or `None` when the job was
    /// skipped by cancellation. Every unresolved target that runs as a worker
    /// still honors single-flight, the D-c cooldown, and the shared rate limiter.
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
                // Per-job pre-fetch cancellation check: a superseded/abandoned
                // block skips its remaining network fetches. Acquire pairs with
                // the Release store on the supersede/disconnect side so a worker
                // that observes the flag also sees everything sequenced before
                // it.
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

    pub fn build_workspace_report(
        &self,
        request: WorkspaceReportRequest,
    ) -> WorkspaceReportResponse {
        self.report_executor
            .install(|| self.build_workspace_report_on_worker(request))
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

        // F4-A: the report aggregation (workspace scan + build_report_rows +
        // summary) runs on a fire-and-forget rayon worker via `spawn_workspace_report`;
        // an uncaught panic here would unwind the pool job and drop the `oneshot`
        // sender, surfacing only a generic transport error. Catch it and return an
        // explicit error response — matching the registry worker (ipc/server) and the
        // per-file report hardening (`analyze_report_source`). `AssertUnwindSafe` is
        // sound: a panic that poisons a cache mutex is handled by the cache's
        // poisoned-lock fallback, and no `&mut` state straddles the boundary.
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
        // Forces an AGGREGATION panic (outside per-file analysis, which is caught
        // separately in `analyze_report_source`) so the `catch_unwind` in
        // `build_workspace_report_on_worker` is exercised. Compiled only under
        // cfg(test); a no-op in release builds.
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
        // One resolver per report run: files sharing a directory share a single
        // .importlensignore ancestor walk, while edits between reports are
        // re-read because each report constructs a fresh resolver.
        let ignore_resolver = IgnoreRuleResolver::default();
        let items = files
            .par_iter()
            .flat_map(|source_path| {
                let source = match fs::read_to_string(source_path) {
                    Ok(source) => source,
                    Err(_) => return Vec::new(),
                };
                self.analyze_report_source(source_path, &request, source, &ignore_resolver)
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

        // Isolate per-file analysis: a panic while analyzing one workspace file
        // must degrade to a skipped file, not fail the entire report. The report
        // runs on a fire-and-forget worker, so an uncaught panic here would tear
        // down the whole scan (mirrors the registry worker's catch_unwind).
        // AssertUnwindSafe is sound because a panic that poisons a cache mutex is
        // already handled by the cache's poisoned-lock fallback.
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

            // WorkspaceReport is a full-workspace scan: read cache non-promoting so it
            // can't flood the recency signal and evict the user's warm set (§5.1).
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
                result: item.result,
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

    pub fn handle_batch(&self, request: BatchRequest) -> BatchResponse {
        if !is_supported_protocol_version(request.version) {
            return protocol_error_batch_response(
                &request,
                format!("unsupported protocol version {}", request.version),
            );
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let mut imports = request
            .imports
            .par_iter()
            .map(|item| self.analyze_with_cache(&context, item, false, ReadIntent::Interactive))
            .collect::<Vec<_>>();
        annotate_shared_bytes(&mut imports);

        BatchResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            indexes: None,
        }
    }

    pub fn handle_batch_streaming<F>(&self, request: BatchRequest, emit_partial: F) -> BatchResponse
    where
        F: Fn(BatchResponse) + Sync,
    {
        if !is_supported_protocol_version(request.version) {
            return protocol_error_batch_response(
                &request,
                format!("unsupported protocol version {}", request.version),
            );
        }

        if request.version < 2 || !request.streaming {
            return self.handle_batch(request);
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let mut imports = request
            .imports
            .par_iter()
            .enumerate()
            .map(|(index, item)| {
                let result =
                    self.analyze_with_cache(&context, item, false, ReadIntent::Interactive);
                emit_partial(BatchResponse {
                    version: request.version,
                    request_id: request.request_id,
                    imports: vec![result.clone()],
                    indexes: Some(vec![index]),
                });
                result
            })
            .collect::<Vec<_>>();
        annotate_shared_bytes(&mut imports);

        BatchResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            indexes: None,
        }
    }

    pub fn handle_file_size(&self, request: FileSizeRequest) -> FileSizeResponse {
        if !(2..=PROTOCOL_VERSION).contains(&request.version) {
            return protocol_error_file_size_response(
                &request,
                format!("unsupported protocol version {}", request.version),
            );
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let mut imports = request
            .imports
            .par_iter()
            .map(|item| self.analyze_with_cache(&context, item, false, ReadIntent::Interactive))
            .collect::<Vec<_>>();
        annotate_shared_bytes(&mut imports);
        let file_size =
            self.file_size_with_cache(&context, &request.active_document_path, &request.imports);

        FileSizeResponse {
            version: request.version,
            request_id: request.request_id,
            raw_bytes: file_size.raw_bytes,
            minified_bytes: file_size.minified_bytes,
            gzip_bytes: file_size.gzip_bytes,
            brotli_bytes: file_size.brotli_bytes,
            zstd_bytes: file_size.zstd_bytes,
            imports,
            error: file_size.error,
            diagnostics: file_size.diagnostics,
        }
    }

    pub fn handle_analyze_document(
        &self,
        request: AnalyzeDocumentRequest,
        ignore_resolver: &IgnoreRuleResolver,
    ) -> AnalyzeDocumentResponse {
        // Interactive analyze (an editor request for the active document the user is
        // looking at) promotes recency. The bulk WorkspaceReport scan reuses this same
        // per-file analysis via `_with_intent(.., ReadIntent::Bulk)` so it does NOT
        // promote — a full-workspace pass can't flood the recency signal (§5.1).
        self.handle_analyze_document_with_intent(request, ignore_resolver, ReadIntent::Interactive)
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

    pub fn handle_file_size_document(
        &self,
        request: FileSizeDocumentRequest,
    ) -> FileSizeDocumentResponse {
        if !(2..=PROTOCOL_VERSION).contains(&request.version) {
            return FileSizeDocumentResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                raw_bytes: 0,
                minified_bytes: 0,
                gzip_bytes: 0,
                brotli_bytes: 0,
                zstd_bytes: 0,
                imports: Vec::new(),
                states: Vec::new(),
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
        let ignore_resolver = IgnoreRuleResolver::default();
        let detected = match detected_imports_for_document(
            &request.active_document_path,
            &request.source,
            true,
            &ignore_resolver,
        ) {
            Ok(imports) => imports,
            Err(error) => {
                return FileSizeDocumentResponse {
                    version: request.version,
                    request_id: request.request_id,
                    raw_bytes: 0,
                    minified_bytes: 0,
                    gzip_bytes: 0,
                    brotli_bytes: 0,
                    zstd_bytes: 0,
                    imports: Vec::new(),
                    states: Vec::new(),
                    error: Some(error.clone()),
                    diagnostics: vec![ImportDiagnostic::for_stage("document_parse", &error)],
                };
            }
        };
        // Interactive size read → serve stale (SWR) unless the client forces fresh
        // (CI / CLI budget checks). A stale serve here triggers the background
        // revalidation + RefreshedResults push in the FileSizeDocument handler.
        let states = self.analysis_items_for_detected(
            &context,
            detected,
            !request.force_fresh,
            ReadIntent::Interactive,
        );
        let requests = states
            .iter()
            .filter_map(|state| state.request.clone())
            .collect::<Vec<_>>();
        let results = states
            .iter()
            .filter_map(|state| state.result.clone())
            .collect::<Vec<_>>();
        let file_size =
            self.file_size_with_cache(&context, &request.active_document_path, &requests);

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
            error: file_size.error,
            diagnostics: file_size.diagnostics,
        }
    }

    /// Background SWR revalidation for a document's sizes: recompute the imports that
    /// were served stale FRESH (bypassing serve-stale), deduped per cache key via a
    /// `RevalidationGuard` so concurrent stale serves coalesce to one recompute and a
    /// panicking recompute cannot leak the in-flight claim. `should_continue` is a
    /// pre-recompute cancellation check (F3-B): a document superseded before/while the
    /// revalidation runs bails before the expensive recompute. After a recompute the
    /// entry's freshness is re-probed and, if it is STILL `Stale` (a dep changed again
    /// mid-recompute), EXACTLY ONE more revalidation is re-armed (F1 — never a loop).
    /// Only the specifiers in `stale_specifiers` are recomputed — a fresh sibling import
    /// in the same document must NOT be re-analyzed, or one changed dep would trigger a
    /// full re-analysis of every import in the file. Returns
    /// `(workspace_root, document_path, fresh_results)` for the client push, or `None`
    /// when nothing was recomputed (parse failure, no stale specifiers, or every stale
    /// key already owned by an in-flight revalidation).
    ///
    /// F2 — no daemon-side debounce. §4.5 asks for background revalidation to be
    /// debounced by `importLens.debounceMs`; that debounce already lives in the CLIENT.
    /// The extension routes every `FileSizeDocument` request through
    /// `DebouncedDocumentScheduler` (see `extension/src/listener.ts`), keyed per document
    /// URI with `config.debounceMs` (default 300ms) and cancel-and-replace semantics, so
    /// the settled requests that reach this per-request revalidation are already
    /// ≥`debounceMs` apart per document. A second per-key debounce here would be
    /// redundant; the in-flight `RevalidationGuard` dedupe below already coalesces the
    /// only remaining concurrency (overlapping requests for the same key).
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
        // F3-B pre-recompute cancellation: a document superseded before this
        // background revalidation starts (a newer FileSizeDocument or prewarm bumped
        // the prefetcher's cancellation generation) bails before any expensive
        // recompute, reusing the prefetch cancellation-generation bailout pattern.
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
        // Index-aligned with `fresh`: each recomputed result is paired with the
        // identity of the import it belongs to, so the client can assign it to the
        // right same-specifier variant instead of collapsing them by specifier.
        let mut identities = Vec::new();
        for detected_import in &detected {
            // Only recompute the imports that were served stale; a fresh sibling has a
            // valid cache entry and re-analyzing it would waste a full bundle+compress.
            if !stale_specifiers.contains(&detected_import.specifier) {
                continue;
            }
            // F3-B: a document superseded mid-iteration bails before the remaining
            // (expensive) recomputes rather than finishing work no client will use.
            if !should_continue() {
                break;
            }
            // Build the request straight from the detected import — do NOT route through
            // analysis_items_for_detected, which would run a full recompute per import
            // just to harvest the request list (doubling work and letting the dedupe
            // gate below fire only after the expensive recompute).
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
            // A served-`Stale` specifier is either a genuine content change (recompute
            // it) or a transient `Unknown` graduated to a quiet `Stale{revalidating}`
            // (§4.3.1). Re-PROBE the raw freshness and NEVER route a graduated `Unknown`
            // into recompute: `analyze_and_cache` would re-read the same locked file, hit
            // the same transient error, and could overwrite the good cached value with an
            // error result. The re-probe itself re-stats the dependency, so for a
            // graduated key it doubles as the active re-check that heals it on a later get.
            if matches!(
                cache.probe_freshness(&key),
                Some(crate::cache::key::Freshness::Unknown)
            ) {
                continue;
            }
            // Dedupe only within one document generation. The real cache key remains
            // global, but the in-flight claim is delivery-scoped so another document
            // importing the same package is not starved of its own refresh push.
            let claim_key = revalidation_claim_key(
                &key,
                &request.workspace_root,
                &request.active_document_path,
                request.analysis_generation,
            );
            let Some(_guard) = cache.begin_revalidation(&claim_key) else {
                continue;
            };
            let mut result = self.analyze_and_cache(
                cache.as_ref(),
                &context,
                &import_request,
                key.clone(),
                resolved.clone(),
                || true,
            );
            // F1 trailing re-check: if a dependency changed AGAIN while this recompute
            // ran, the value just inserted already reflects the older state and
            // `probe_freshness` re-stats it to `Stale`. A concurrent stale serve during
            // the recompute was coalesced away by the in-flight guard, so nothing else
            // heals it until the next interactive read. Re-arm EXACTLY ONE more
            // revalidation (never a loop — a still-`Stale` second result is left for the
            // next interactive read) so the served value catches up to the newer state.
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

        // Resolve each dependency's installed version (an ancestor walk plus a
        // package.json read) in parallel; into_par_iter preserves order, so the
        // resulting states and import_requests still line up with streaming
        // indexes exactly as the sequential loop did.
        type PreparedDependency = (ImportRequest, Option<ResolvedPackage>);
        let resolution_started_at = Instant::now();
        let resolved: Vec<(
            PackageJsonDependencyAnalysisItem,
            Option<PreparedDependency>,
        )> = entries
            .into_par_iter()
            .map(|entry| {
                // Resolve the package once here and carry the ResolvedPackage
                // to the analysis pass below, so the manifest is read once
                // instead of resolve_installed_package_version + a second
                // resolve_package_entry per dependency. Entry resolution can
                // fail for an installed-but-unresolvable package (e.g. types
                // -only); fall back to the lightweight version read so the
                // analysis pass still applies its declaration-only handling.
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
                            (Ok(version), Some(resolved))
                        }
                        Err(_) => (
                            resolve_installed_package_version(
                                &context.active_document_path,
                                &entry.name,
                            ),
                            None,
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

        enum PendingPackageJsonAnalysis {
            Resolved {
                import_request: ImportRequest,
                resolved: ResolvedPackage,
                cache_key: String,
            },
            Unresolved {
                import_request: ImportRequest,
            },
        }

        enum PackageJsonCacheClassification {
            Cached {
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

                let Some(resolved) = resolved else {
                    return Some(PackageJsonCacheClassification::Pending {
                        index,
                        analysis: PendingPackageJsonAnalysis::Unresolved {
                            import_request: import_request.clone(),
                        },
                    });
                };

                let (cache_key, cached_result) = fresh_cached_result_for_resolved_import(
                    package_cache.as_ref(),
                    import_request,
                    resolved,
                    ReadIntent::Interactive,
                );
                if let Some(result) = cached_result {
                    return Some(PackageJsonCacheClassification::Cached { index, result });
                }

                Some(PackageJsonCacheClassification::Pending {
                    index,
                    analysis: PendingPackageJsonAnalysis::Resolved {
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
                PackageJsonCacheClassification::Cached { index, result } => {
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
        let analyzed_results = pending_analysis
            .into_par_iter()
            .map(|(index, pending)| {
                let result = match pending {
                    PendingPackageJsonAnalysis::Resolved {
                        import_request,
                        resolved,
                        cache_key,
                    } => self.analyze_and_cache(
                        package_cache.as_ref(),
                        &context,
                        &import_request,
                        cache_key,
                        resolved,
                        || true,
                    ),
                    PendingPackageJsonAnalysis::Unresolved { import_request } => self
                        .analyze_with_cache(
                            &context,
                            &import_request,
                            false,
                            ReadIntent::Interactive,
                        ),
                };

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
            })
            .collect::<Vec<_>>();
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
        annotate_shared_bytes(&mut results);
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

        let response = self.enumerate_exports(EnumerateExportsRequest {
            message_type: "enumerate_exports".to_owned(),
            version: request.version,
            request_id: request.request_id,
            workspace_root: request.workspace_root,
            active_document_path: request.active_document_path,
            specifier: context.specifier.clone(),
            package_name,
            package_version,
        });

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
            runtime: ImportRuntime::Component,
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

        let graph = match build_module_graph_cached(&resolved.entry_path) {
            Ok(graph) => graph,
            Err(error) => {
                return EnumerateExportsResponse {
                    version: request.version,
                    request_id: request.request_id,
                    specifier: request.specifier,
                    exports: Vec::new(),
                    error: Some(error.clone()),
                    diagnostics: vec![ImportDiagnostic {
                        stage: "module_graph".to_owned(),
                        message: error,
                        details: vec![format!("entry_path: {}", resolved.entry_path.display())],
                    }],
                };
            }
        };

        let exports = module_exported_names(&graph, graph.entry_id, true);

        EnumerateExportsResponse {
            version: request.version,
            request_id: request.request_id,
            specifier: request.specifier,
            exports,
            error: None,
            diagnostics: graph
                .diagnostics
                .iter()
                .map(|diagnostic| ImportDiagnostic {
                    stage: diagnostic.stage.clone(),
                    message: diagnostic.message.clone(),
                    details: diagnostic.details.clone(),
                })
                .collect(),
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
            // A single serialized-length measurement of the shared registry
            // snapshot (D-b's envelope size), not a scan.
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
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

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
                // "Clear everything" drops the shared npm-hint store and the
                // shared resolver caches in addition to the bundle shards, so no
                // derived state survives the clear (X-14/X-16). The L1/graph
                // caches are cleared unconditionally below (X-21).
                self.registry_hints.clear();
                crate::pipeline::resolver::invalidate_shared_resolvers();
                removed
            }
            CacheRemoveScope::Registry => {
                // Registry-only: drop the npm-hint store and nothing else. Bundle
                // shards and their derived L1/graph caches stay put, so this
                // returns no shard-removal results.
                self.registry_hints.clear();
                Vec::new()
            }
            CacheRemoveScope::Orphans => {
                // Manual "Remove Orphaned Caches" (RB-17): drive-safe shard reclaim
                // for moved/deleted projects + a stale-entry scrub of surviving
                // shards, plus a stale-registry-metadata prune. The maintenance tick
                // runs the shard-only half of this automatically (throttled); this
                // button is the on-demand, entry-inclusive pass.
                let registry_removed = self.registry_hints.purge_expired_metadata();
                if registry_removed > 0 {
                    crate::logging::log_debug(
                        "registry",
                        format!("orphan purge dropped {registry_removed} stale registry entries"),
                    );
                }
                self.cache_registry.purge_orphans()
            }
        };
        let (removed, failed): (Vec<_>, Vec<_>) =
            results.into_iter().partition(|result| result.removed);

        if matches!(request.scope, CacheRemoveScope::Orphans) {
            // An entry-only orphan purge (uninstalled package, project still
            // present) removes no shards, so the blanket clear below doesn't fire.
            // Drop the L1/graph entries whose paths are specifically gone.
            crate::pipeline::file_size_cache::shared_file_size_cache().purge_missing_paths();
            crate::pipeline::graph::purge_missing_module_graphs();
            crate::pipeline::cjs::purge_missing_cjs_module_sets();
        }

        // Drop the derived L1/graph caches when a store-clearing scope ran. `All`
        // clears them UNCONDITIONALLY (X-21): a "Clear everything" that removed no
        // shard (nothing was cached yet, or only the registry was populated) must
        // still drop the derived caches so no stale derived state survives. Scoped
        // shard removals still only pay this when they actually removed a shard;
        // the registry-only scope leaves these caches untouched.
        if matches!(request.scope, CacheRemoveScope::All) || !removed.is_empty() {
            clear_module_graph_cache();
            crate::pipeline::cjs::clear_cjs_module_cache();
            // Drop L1 aggregate sizes too so the status-bar size recomputes fresh
            // after a cache clear (the memory-only L1 is not generation-bumped here).
            crate::pipeline::file_size_cache::shared_file_size_cache().clear();
        }

        // A just-cleared store must not be silently repopulated as "fresh" by an
        // analysis that captured the pre-clear generation. Bump so any in-flight
        // insert lands `verified_generation < current` and re-validates (X-17).
        crate::cache::memory::bump_cache_generation();

        CacheRemoveResponse {
            version: request.version,
            request_id: request.request_id,
            removed,
            failed,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.cache_registry.invalidate_package(package_name);
        invalidate_module_graph_cache_for_package(package_name);
        crate::pipeline::cjs::invalidate_cjs_module_cache_for_package(package_name);
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
    }

    pub fn invalidate_all(&self) {
        self.cache_registry.clear_all();
        clear_module_graph_cache();
        crate::pipeline::cjs::clear_cjs_module_cache();
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
    }

    /// Periodic cache maintenance: enforce the global disk-byte budget by
    /// evicting the least-recently-used entries across shards, then reclaim
    /// fragmented shard files. Runs on the maintenance interval task via
    /// `spawn_blocking` — never on the connection's async loop.
    ///
    /// The maintenance task's first tick fires immediately after Hello, so this
    /// also serves as the daemon's STARTUP maintenance pass; subsequent ticks are
    /// the periodic pass. The registry-store retention + size cap ride the same
    /// seam (A5/X-15, D3+D4 / §6.1): they must not run on the write hot path, so
    /// they run here rather than on every registry write.
    pub fn run_cache_maintenance(&self) {
        // F4-B skip-if-running: a redundant concurrent pass (e.g. a re-Hello's new
        // maintenance task first tick overlapping the previous connection's still-
        // running detached pass) is a no-op. Passes already serialize on redb's
        // single writer; this avoids the wasted duplicate scan/compaction. The guard
        // clears the flag on drop (including on panic), so a failed pass never wedges
        // maintenance off permanently.
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

        // Registry metadata store: automatic 30-day retention + byte-budget size
        // cap, both written authoritatively so the deletions stick. The byte budget
        // is the user's `importLens.registryCacheMaxSizeMB`, negotiated at Hello and
        // stored here (RB-16); it falls back to the daemon default for an older
        // client that omits the field (serde-defaulted in `HelloMessage`).
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

        // Orphaned-shard reclaim (RB-17): a project that was moved/deleted is never
        // reopened, so the on-access reclaim (name invalidation + `Gone` eviction)
        // never reaches it and its whole shard lingers — reclaimed here instead.
        // Drive-safe (an offline/unplugged drive keeps its shard, X-3) and throttled
        // (`ORPHAN_SWEEP_INTERVAL`), so most ticks are a cheap no-op. Removing a shard
        // strands its derived L1/graph entries, so clear those + bump the generation,
        // exactly as the manual cache-remove path does.
        let orphans_removed = self
            .cache_registry
            .sweep_orphaned_shards_if_due()
            .iter()
            .filter(|result| result.removed)
            .count();
        if orphans_removed > 0 {
            clear_module_graph_cache();
            crate::pipeline::cjs::clear_cjs_module_cache();
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

    pub fn prewarm_import<F>(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        should_continue: F,
    ) where
        F: Fn() -> bool,
    {
        let Ok(resolved) = resolve_package_entry(&context.active_document_path, request) else {
            return;
        };
        self.prewarm_resolved_import(context, request, resolved, should_continue);
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

        // Prewarm dedup check: a bulk/background read must not promote recency
        // (scan resistance, design §5.1) — prewarming the whole document should not
        // evict the user's warm working set.
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
                // A path we can't map to a package name is opaque, but the
                // presence of one odd path (pnpm's `.pnpm/…` store, a symlinked
                // package, or any layout the mapper doesn't recognize) is not a
                // reason to nuke every OTHER project's cache -- skip it and keep
                // targeting whatever did map.
                None => crate::logging::log_debug(
                    "cache",
                    format!(
                        "unmappable package.json path, skipping targeted invalidation: {package_json_path}"
                    ),
                ),
            }
        }

        if package_names.is_empty() {
            // Nothing mapped. An empty batch is a no-op (unchanged); a
            // non-empty batch where every path was opaque has no safe
            // targeted fallback, so a full clear is the only safe option.
            if package_json_paths.is_empty() {
                return false;
            }
            self.invalidate_all();
            return true;
        }

        // Even for a large burst, invalidate only the affected packages (a single
        // decode pass via `invalidate_packages`) rather than nuking every project
        // shard under this workspace's cache base -- a full clear would evict
        // unrelated sibling projects in a multi-root / monorepo window. The
        // graph/resolver/generation invalidations run once for the whole burst.
        self.cache_registry.invalidate_packages(&package_names);
        for package_name in &package_names {
            invalidate_module_graph_cache_for_package(package_name);
            crate::pipeline::cjs::invalidate_cjs_module_cache_for_package(package_name);
        }
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
        true
    }

    fn analysis_items_for_detected(
        &self,
        context: &AnalysisContext,
        detected: Vec<DetectedImport>,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> Vec<ImportAnalysisItem> {
        let mut items = detected
            .into_par_iter()
            .map(|detected| {
                match import_request_for_detected(&context.active_document_path, &detected) {
                    Ok(request) => ImportAnalysisItem {
                        result: Some(self.analyze_with_cache(
                            context,
                            &request,
                            serve_stale,
                            intent,
                        )),
                        detected,
                        status: ImportAnalysisStatus::Ready,
                        message: None,
                        request: Some(request),
                    },
                    Err(message) => ImportAnalysisItem {
                        detected,
                        status: ImportAnalysisStatus::Missing,
                        message: Some(message),
                        request: None,
                        result: None,
                    },
                }
            })
            .collect::<Vec<_>>();

        let mut results = items
            .iter_mut()
            .filter_map(|item| item.result.take())
            .collect::<Vec<_>>();
        annotate_shared_bytes(&mut results);
        let mut result_iter = results.into_iter();
        for item in &mut items {
            if item.status == ImportAnalysisStatus::Ready {
                item.result = result_iter.next();
            }
        }

        items
    }

    // L1 aggregate cache: return the cached FileSizeComputation when the file's
    // import set is unchanged (and node_modules has not been invalidated),
    // otherwise recompute once and overwrite this document's single slot.
    fn file_size_with_cache(
        &self,
        context: &AnalysisContext,
        active_document_path: &str,
        requests: &[ImportRequest],
    ) -> crate::pipeline::file_size::FileSizeComputation {
        let cache = crate::pipeline::file_size_cache::shared_file_size_cache();
        let path = PathBuf::from(active_document_path);
        let signature = crate::pipeline::file_size_cache::file_size_signature(context, requests);

        if let Some(hit) = cache.get(&path, signature) {
            crate::logging::log_debug("file_size_cache", format!("hit: {}", path.display()));
            return hit;
        }

        crate::logging::log_debug("file_size_cache", format!("miss: {}", path.display()));
        let computed = compute_file_size(context, requests);
        cache.insert(path, signature, computed.clone());
        computed
    }

    fn analyze_with_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> ImportResult {
        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => {
                self.analyze_resolved_with_cache(context, request, resolved, serve_stale, intent)
            }
            Err(_) => analyze_import(context, request),
        }
    }

    // Cache lookup + analysis for an already-resolved package. The package.json
    // analysis path resolves each dependency once and reuses the ResolvedPackage
    // here, avoiding a second resolve_package_entry (and its manifest read).
    //
    // `serve_stale` selects the read semantics: interactive size reads serve the
    // last-known value instantly (flagged on the result's `freshness`) via
    // stale-while-revalidate; batch/CI reads pass `false` so a changed dependency is
    // recomputed synchronously and never served stale (spec §4.5).
    fn analyze_resolved_with_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        resolved: ResolvedPackage,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> ImportResult {
        let key = cache_key_for_resolved_import(request, &resolved);
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);

        if serve_stale {
            let lookup_started_at = Instant::now();
            // SWR: serve the last-known value (flagged Stale/Unverified) instead of
            // evicting-and-recomputing. The FileSizeDocument handler spawns a background
            // recompute + push when a served result is Stale. A bulk read
            // (WorkspaceReport, Compare) uses the non-promoting variant so a
            // full-workspace scan can't flood the recency signal (scan resistance, §5.1).
            let served = match intent {
                ReadIntent::Interactive => cache.get_with_result_freshness(&key),
                ReadIntent::Bulk => cache.get_with_result_freshness_for_bulk(&key),
            };
            if let Some((result, _freshness)) = served {
                log_cache_lookup_timing(
                    request,
                    cache_read_mode_label(serve_stale, intent),
                    true,
                    Some(&result),
                    lookup_started_at.elapsed(),
                );
                return result;
            }
            log_cache_lookup_timing(
                request,
                cache_read_mode_label(serve_stale, intent),
                false,
                None,
                lookup_started_at.elapsed(),
            );
        } else {
            // Force-fresh (CI / `importlens check`, §4.5): serve ONLY a value verified
            // `Fresh` against disk, across BOTH the memory working set and the disk
            // cache. `get_if_fresh` returns `None` on Unknown/Stale/Gone/miss, so a
            // transient `Unknown` — which the evicting `get` would launder into a
            // `cache_hit`, whether memory-resident OR cold-daemon disk-hydrated — never
            // reaches CI; we recompute synchronously below instead. This single gate
            // also removes the double dependency re-verification of the prior
            // memory-only `probe_freshness` + `get`.
            let cached = fresh_cached_result_for_key(cache.as_ref(), request, &key, intent);
            if let Some(result) = cached {
                return result;
            }
        }

        self.analyze_and_cache(cache.as_ref(), context, request, key, resolved, || true)
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
                    analyze_resolved_import_with_graph(context, request, resolved.clone());
                let dependency_fingerprints = if should_cache_result(&result) {
                    dependency_fingerprints(&resolved, analyzed_graph.as_ref(), request.runtime)
                } else {
                    Vec::new()
                };

                ComputedAnalysis {
                    result,
                    dependency_fingerprints,
                }
            });

        if should_cache_result(&computed.result) && should_store() {
            self.cache_full_variant_alias(
                cache,
                request,
                &computed.result,
                &resolved,
                &computed.dependency_fingerprints,
                captured_generation,
            );
            cache.insert_with_fingerprints_at_generation(
                key,
                computed.result.clone(),
                computed.dependency_fingerprints.clone(),
                captured_generation,
            );
        }

        computed.result
    }

    fn cache_full_variant_alias(
        &self,
        cache: &ImportCache,
        request: &ImportRequest,
        result: &ImportResult,
        resolved: &ResolvedPackage,
        dependency_fingerprints: &[crate::cache::key::FileFingerprint],
        verified_generation: u64,
    ) {
        if !resolved.side_effects.has_side_effects()
            || result.is_cjs
            || has_request_specific_diagnostics(result)
            || matches!(
                request.import_kind,
                ImportKind::Namespace | ImportKind::Dynamic
            )
        {
            return;
        }

        let mut namespace_request = request.clone();
        namespace_request.import_kind = ImportKind::Namespace;
        namespace_request.named.clear();
        let namespace_key = cache_key_for_resolved_import(&namespace_request, resolved);

        if cache.get(&namespace_key).is_some() {
            return;
        }

        let mut namespace_result = result.clone();
        namespace_result.cache_hit = false;
        namespace_result.truly_treeshakeable = false;
        cache.insert_with_fingerprints_at_generation(
            namespace_key,
            namespace_result,
            dependency_fingerprints.to_vec(),
            verified_generation,
        );
    }
}

/// Test-only handle: exists so integration tests can seed cached registry
/// metadata via `ImportLensService::registry_hints_for_tests`. See that
/// method's doc comment for why this cannot be `#[cfg(test)]`-gated.
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

fn should_cache_result(result: &ImportResult) -> bool {
    result.error.is_none() && !has_request_specific_diagnostics(result)
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
    let result = cache.get_if_fresh(key);
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

fn has_request_specific_diagnostics(result: &ImportResult) -> bool {
    result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.stage == "exports")
}

fn dependency_fingerprints(
    resolved: &ResolvedPackage,
    graph: Option<&std::sync::Arc<ModuleGraph>>,
    runtime: crate::ipc::protocol::ImportRuntime,
) -> Vec<crate::cache::key::FileFingerprint> {
    use crate::cache::key::file_fingerprint_reading_hash;

    // No analyzed graph (CJS, oversized entry, or static fallback): the result was
    // not computed from a module graph, so freshness is pinned to the manifest and
    // entry — read+hashed here (RB-2) so an equal-length, mtime-preserving edit is
    // still detected rather than probing Fresh forever.
    let Some(graph) = graph else {
        let mut fingerprints: Vec<crate::cache::key::FileFingerprint> = [
            resolved.package_root.join("package.json"),
            resolved.entry_path.clone(),
        ]
        .into_iter()
        .filter_map(file_fingerprint_reading_hash)
        .collect();

        // A CJS package has no `ModuleGraph`, but the analyzer cached read-time
        // fingerprints for every transitively `require()`d module (RB-5). Fold them in
        // so a first-party CJS dep edit invalidates instead of probing Fresh against
        // manifest+entry alone. The fingerprints carry read-time content hashes, so the
        // strict per-get gate hash-verifies first-party CJS modules. (A CJS result that
        // fell back to static-entry sizing may pull in a partial/unused module set —
        // harmless over-coverage, never a stale-serve.)
        if resolved.is_cjs
            && let Some(cjs_fingerprints) =
                crate::pipeline::cjs::cjs_module_fingerprints(&resolved.entry_path, runtime)
        {
            fingerprints.extend(cjs_fingerprints);
        }
        fingerprints.sort_by(|a, b| a.path.cmp(&b.path));
        fingerprints.dedup_by(|a, b| a.path == b.path);
        return fingerprints;
    };

    let mut paths = vec![
        // The manifest is not a graph module, so fingerprints_with_content_hashes
        // read+hashes it (RB-2) rather than degrading to mtime+len — a same-content
        // touch no longer re-verifies, and a same-length edit is still caught.
        resolved.package_root.join("package.json"),
        resolved.entry_path.clone(),
    ];
    paths.extend(graph.modules.iter().map(|module| module.path.clone()));
    paths.extend(graph.dependency_paths.iter().cloned());
    // Content hashes come from the EXACT graph instance the result was computed
    // from (threaded out of analysis), so these fingerprints describe the
    // analyzed bytes with no second fetch that could rebuild against a dependency
    // that changed during the analysis window (Finding 4). The pre-analysis
    // generation gate still backstops the node_modules-changed case.
    crate::pipeline::graph::fingerprints_with_content_hashes(paths, graph)
}

pub fn protocol_error_batch_response(request: &BatchRequest, message: String) -> BatchResponse {
    BatchResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        imports: request
            .imports
            .iter()
            .map(|item| protocol_error(item, message.clone()))
            .collect(),
        indexes: None,
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

pub fn protocol_error_file_size_response(
    request: &FileSizeRequest,
    message: String,
) -> FileSizeResponse {
    FileSizeResponse {
        version: request.version.min(PROTOCOL_VERSION),
        request_id: request.request_id,
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        imports: request
            .imports
            .iter()
            .map(|item| protocol_error(item, message.clone()))
            .collect(),
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: "protocol".to_owned(),
            message,
            details: Vec::new(),
        }],
    }
}

fn protocol_error(request: &ImportRequest, message: String) -> ImportResult {
    ImportResult {
        freshness: crate::ipc::protocol::ResultFreshness::fresh(),
        specifier: request.specifier.clone(),
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        cache_hit: false,
        side_effects: true,
        truly_treeshakeable: false,
        is_cjs: false,
        confidence: ConfidenceLevel::Low,
        confidence_reasons: vec![
            "Protocol validation failed before a bundle size could be measured.".to_owned(),
        ],
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: "protocol".to_owned(),
            message,
            details: vec![format!("specifier: {}", request.specifier)],
        }],
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
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
                per_file_brotli_bytes: None,
            },
        };

        // The `__IMPORTLENS_FORCE_PANIC__` sentinel (compiled in only under
        // cfg(test)) makes the per-file analysis panic. A single bad file must
        // be isolated - skipped from the report - rather than failing the whole
        // workspace scan.
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

    // F1: the trailing re-check re-arms EXACTLY ONE more revalidation, and only when
    // the entry is still `Stale` after a recompute. A fully deterministic
    // mid-recompute timing repro is impractical — the second dependency change must
    // land inside the synchronous recompute window, and freshness is content-hash
    // based — so the DECISION is tested directly: `Stale` -> re-arm; every other
    // outcome -> no re-run. The re-run is one-shot by construction (a straight-line
    // second `analyze_and_cache`, not a loop), so a still-`Stale` second result is
    // left for the next interactive read rather than spinning.
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

    // F4-B: while a maintenance pass holds the in-progress claim, a second
    // concurrent `run_cache_maintenance` must be a no-op. The old-detached-pass vs
    // re-Hello-new-pass race is not deterministically reproducible, so the flag
    // decision is tested directly: the claim is exclusive while held and frees on
    // drop so the next pass can proceed.
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

    // F4-A: a panic in the report AGGREGATION (outside per-file analysis) must
    // surface as an explicit error response through the fire-and-forget spawn path,
    // rather than unwinding the rayon job and dropping the `oneshot` sender.
    #[test]
    fn workspace_report_aggregation_panic_yields_error_response() {
        let service = std::sync::Arc::new(ImportLensService::new(None, false));
        let (tx, rx) = tokio::sync::oneshot::channel();
        service.spawn_workspace_report(
            WorkspaceReportRequest {
                message_type: "workspace_report".to_owned(),
                version: PROTOCOL_VERSION,
                request_id: 77,
                // Sentinel (compiled only under cfg(test)) panics inside the
                // aggregation, exercising the catch_unwind.
                workspace_root: "__IMPORTLENS_FORCE_REPORT_PANIC__".to_owned(),
                budgets: WorkspaceReportBudgets {
                    per_import_brotli_bytes: None,
                    per_file_brotli_bytes: None,
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
mod analyze_and_cache_graph_reuse_tests {
    use super::{ImportLensService, should_cache_result};
    use crate::cache::key::cache_key_for_resolved_import;
    use crate::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
    use crate::pipeline::analyze::AnalysisContext;
    use crate::pipeline::graph::graph_fetch_probe;
    use crate::pipeline::resolver::{ResolvedPackage, SideEffectsMode};
    use std::fs;

    /// Finding 4 regression: `analyze_and_cache` must build content-hash
    /// fingerprints from the SAME module graph instance it analyzed, never
    /// re-fetch the graph by key. The probe counts
    /// `build_module_graph_cached_with_runtime` calls for the analyzed entry:
    /// the fix makes that exactly one (analysis only); the pre-fix code fetched
    /// a second time inside `dependency_fingerprints`, which — if a dependency
    /// changed during the analysis window — pairs a stale result with
    /// fresh-looking fingerprints and serves it `Fresh`.
    #[test]
    fn analyze_and_cache_fetches_module_graph_once() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let workspace =
            std::env::temp_dir().join(format!("il-a4-graph-reuse-{}-{unique}", std::process::id()));
        let package_root = workspace.join("node_modules").join("pkg-a4");
        fs::create_dir_all(&package_root).expect("package root");
        // ESM entry + one internal dependency so the OXC pipeline builds a real
        // multi-module graph (`Some(graph)`), exercising the content-hash path.
        fs::write(
            package_root.join("index.mjs"),
            "export { value } from './dep.mjs';\n",
        )
        .expect("entry");
        fs::write(
            package_root.join("dep.mjs"),
            "export const value = 41;\nexport const spare = 7;\n",
        )
        .expect("dep");
        fs::write(package_root.join("package.json"), "{\"name\":\"pkg-a4\"}").expect("manifest");

        let entry_path = package_root.join("index.mjs");
        let resolved = ResolvedPackage {
            package_root: package_root.clone(),
            package_json: serde_json::json!({ "name": "pkg-a4", "version": "1.0.0" }),
            entry_path: entry_path.clone(),
            is_cjs: false,
            side_effects: SideEffectsMode::False,
        };
        let request = ImportRequest {
            specifier: "pkg-a4".to_owned(),
            package_name: "pkg-a4".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        };
        let context = AnalysisContext {
            workspace_root: workspace.clone(),
            active_document_path: workspace.join("src").join("app.ts"),
        };

        // Arm the probe on the exact entry both analysis and (pre-fix)
        // fingerprinting fetch. The path is unique to this test, so concurrent
        // sibling tests building other graphs never perturb the count.
        graph_fetch_probe::arm(entry_path.clone());

        let service = ImportLensService::new(None, false);
        let cache = service
            .cache_registry
            .cache_for_root(&context.workspace_root);
        let key = cache_key_for_resolved_import(&request, &resolved);
        let result =
            service.analyze_and_cache(cache.as_ref(), &context, &request, key, resolved, || true);

        let hits = graph_fetch_probe::hits();
        graph_fetch_probe::disarm();
        fs::remove_dir_all(&workspace).ok();

        assert_eq!(result.error, None, "analysis should succeed: {result:?}");
        assert!(
            should_cache_result(&result),
            "result must reach the fingerprint path or the fetch count is vacuous",
        );
        assert_eq!(
            hits, 1,
            "analyze_and_cache must reuse the analyzed graph for fingerprints \
             (one fetch); a second fetch is the Finding 4 TOCTOU",
        );
    }
}

#[cfg(test)]
mod analyze_and_cache_single_flight_tests {
    use super::{ComputedAnalysis, ImportLensService};
    use crate::cache::key::cache_key_for_resolved_import;
    use crate::ipc::protocol::{
        ConfidenceLevel, ImportKind, ImportRequest, ImportResult, ImportRuntime, ResultFreshness,
    };
    use crate::pipeline::analyze::AnalysisContext;
    use crate::pipeline::resolver::{ResolvedPackage, SideEffectsMode};
    use std::{
        sync::{Arc, Condvar, Mutex, mpsc},
        thread,
        time::Duration,
    };

    fn cacheable_result(specifier: &str) -> ImportResult {
        ImportResult {
            specifier: specifier.to_owned(),
            raw_bytes: 42,
            minified_bytes: 21,
            gzip_bytes: 10,
            brotli_bytes: 8,
            zstd_bytes: 9,
            cache_hit: false,
            side_effects: true,
            truly_treeshakeable: false,
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
            cache.get(&key).is_some(),
            "a follower with should_store=true must keep its cache write even when the leader did not store",
        );
    }
}
