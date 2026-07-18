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
        BatchRequest, BatchResponse, CacheListRequest, CacheListResponse, CacheRemoveRequest,
        CacheRemoveResponse, CacheRemoveScope, CacheStatusRequest, CacheStatusResponse,
        CompleteImportMembersRequest, CompleteImportMembersResponse, DetectedImport,
        EnumerateExportsRequest, EnumerateExportsResponse, FileSizeDocumentRequest,
        FileSizeDocumentResponse, FileSizeRequest, FileSizeResponse, FreshnessKind,
        ImportAnalysisItem, ImportAnalysisStatus, ImportDiagnostic, ImportKind, ImportRequest,
        ImportResult, ImportRuntime, ImportSyntax, PROTOCOL_VERSION,
        PackageJsonDependencyAnalysisItem, RefreshedImportIdentity,
        RegistryHintMode as ProtocolRegistryHintMode, RegistryHintResult, RegistryHintTarget,
        WorkspaceReportRequest, WorkspaceReportResponse, WorkspaceReportSummary,
        is_supported_protocol_version,
    },
    pipeline::analyze::{
        AnalysisContext, analyze_import, analyze_resolved_import_with_dependencies,
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

/// The outcome of a cache lookup, before any engine build has been attempted.
///
/// Separating "did the cache answer this?" from "build it" is what lets a batch
/// classify every import at pool width and hand only the misses to the two-permit
/// engine drain. `Miss` carries the resolved package and cache key forward so the
/// build half does not re-read the manifest.
enum CacheProbe {
    Hit(Box<ImportResult>),
    /// Boxed: this rides in the `Err` arm of the classify closure for every import in
    /// a batch, and `ResolvedPackage` dwarfs the discriminant.
    Miss(Box<PendingBuild>),
    /// The specifier did not resolve to a package; the static path handles it, and it
    /// may still build, so it is not a hit.
    Unresolved,
}

struct PendingBuild {
    resolved: ResolvedPackage,
    key: String,
}

/// One import a streamed response answered `Loading`: everything needed to build it after the
/// response has already gone out, and to address the result back to the right import on the
/// client (the identity — specifier alone is not unique, since two imports of one package differ
/// by kind and named exports).
pub struct PendingImport {
    detected: DetectedImport,
    request: ImportRequest,
    pending: Box<PendingBuild>,
}

/// One import a response already carried a real measurement for, addressed by the same identity a
/// push uses.
///
/// The streamed builds need these: shared-module bytes are a property of the WHOLE document (which
/// modules two imports both pull in), so the final annotation pass cannot see only the imports that
/// arrived late.
///
/// The import's runtime rides in on `identity`, which needs it anyway (two runtime variants of one
/// Astro import statement are two rows, and a specifier+kind+named key collides them into one), so
/// it is not repeated as a second field here. Both construction sites used to drop the runtime on
/// the floor, which is why the closing shared-bytes pass could not see the boundary at all.
///
/// **There IS a copy, and an earlier version of this comment denied it.** `ImportRuntime` lives on
/// two structs, and the two partitions read different ones: SHARING partitions on
/// `DetectedImport.runtime` (via this identity), while the BUILD partitions on
/// `ImportRequest.runtime` (`pipeline::file_size` does `groups.entry(request.runtime)`). What keeps
/// them from disagreeing is not the absence of a second field — it is that there is exactly one
/// SOURCE (`DetectedImport.runtime`, decided in `document::script_regions`) and exactly one
/// DERIVATION that copies it forward ([`import_request_for_detected`]). That is a fine design. It
/// is not the same claim, and the difference is not academic: the derivation was the unpinned link,
/// and rewriting that one line to a constant left the whole daemon suite green while a
/// mixed-runtime file silently under-reported by ~49%. What makes the copy safe is the test that
/// now pins it (`tests/file_size_runtime.rs::a_mixed_runtime_astro_document_is_built_as_two_artifacts`),
/// not an assertion that the copy does not exist.
#[derive(Clone)]
pub struct MeasuredImport {
    pub result: ImportResult,
    pub identity: RefreshedImportIdentity,
}

/// A document analysis that did not wait for the engine: what the cache could answer now, what is
/// still to be built, and — for the shared-bytes pass that closes the document — the measurements
/// the response already carried.
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
        // Reading, parsing and import-detecting a file needs no engine permit, so this
        // runs at the width of the report's own pool rather than the engine's build
        // width — the engine drain lives one level down, around the misses only. The
        // pool is dedicated to reports (`report_executor`), so widening here cannot
        // starve interactive requests on the global pool.
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
            //
            // The report is the one caller that still WAITS for every build: its rows are a
            // table, and a row that says "still measuring" is not a row. It therefore keeps the
            // complete (blocking) analysis, and a workspace naming enough parked packages can
            // still outlive the client's 300s — stated as such in the SRS rather than papered
            // over with a fabricated size.
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
        let mut imports = self.analyze_batch(&context, &request.imports, |_, _| {});
        annotate_shared_bytes(runtimes_of(&request.imports).zip(imports.iter_mut()));

        BatchResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            indexes: None,
        }
    }

    /// Cache hits are classified across the Rayon pool; only misses queue for the
    /// two-permit engine drain. `emit` fires once per import as it settles, in
    /// completion order, and carries the import's original index.
    fn analyze_batch(
        &self,
        context: &AnalysisContext,
        imports: &[ImportRequest],
        emit: impl Fn(usize, &ImportResult) + Sync,
    ) -> Vec<ImportResult> {
        drain_classified(
            imports,
            |index, item| match self.probe_cache(context, item, false, ReadIntent::Interactive) {
                CacheProbe::Hit(result) => {
                    emit(index, &result);
                    Ok(*result)
                }
                pending => Err(pending),
            },
            |index, item, pending| {
                let result = self.complete_probe(context, item, pending);
                emit(index, &result);
                result
            },
        )
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
        let mut imports = self.analyze_batch(&context, &request.imports, |index, result| {
            emit_partial(BatchResponse {
                version: request.version,
                request_id: request.request_id,
                imports: vec![result.clone()],
                indexes: Some(vec![index]),
            });
        });
        annotate_shared_bytes(runtimes_of(&request.imports).zip(imports.iter_mut()));

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
        let mut imports = self.analyze_batch(&context, &request.imports, |_, _| {});
        annotate_shared_bytes(runtimes_of(&request.imports).zip(imports.iter_mut()));
        // Hand the per-import measurements to the aggregate: if its combined build fails, the
        // conservative fallback sums THESE instead of re-analyzing every import through the
        // engine a second time.
        let sized = request
            .imports
            .iter()
            .cloned()
            .zip(imports.iter().cloned())
            .map(|(request, result)| SizedImport::installed(request, Some(result)))
            .collect::<Vec<_>>();
        let file_size = self.file_size_with_cache(&context, &request.active_document_path, &sized);

        FileSizeResponse {
            version: request.version,
            request_id: request.request_id,
            raw_bytes: file_size.raw_bytes,
            minified_bytes: file_size.minified_bytes,
            gzip_bytes: file_size.gzip_bytes,
            brotli_bytes: file_size.brotli_bytes,
            zstd_bytes: file_size.zstd_bytes,
            imports,
            incomplete: file_size.incomplete,
            degraded: file_size.degraded,
            error: file_size.error,
            diagnostics: file_size.diagnostics,
        }
    }

    /// Analyze a document and WAIT for every miss to build.
    ///
    /// The complete answer, at the price of the client waiting for the slowest build in the
    /// document. Only two callers want that trade: the workspace report (a table row cannot say
    /// "still measuring") and `importlens check` through the force-fresh file-size path (CI must
    /// judge the real number or fail loudly). The editor takes
    /// [`Self::handle_analyze_document_streaming`] instead.
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

    /// Analyze a document WITHOUT waiting for any engine build.
    ///
    /// This is the editor's path, and the whole point of the redesign. The response carries
    /// every import the cache could answer, plus a `Loading` placeholder for each one whose
    /// build has yet to run — and it is returned at once, because the response no longer waits
    /// for the engine at all. The pending builds come back in
    /// [`StreamedDocumentAnalysis::pending`]; the IPC server runs them and pushes each result to
    /// the client as it lands (`RefreshedResults`).
    ///
    /// What that buys: one package that parks the bundler delays exactly one import's number.
    /// It used to delay the whole `AnalyzeDocumentResponse` past the client's 10s deadline, and
    /// the client threw the entire document away — every import in it already answered from
    /// cache included.
    ///
    /// The placeholder is load-bearing, not cosmetic. An import ABSENT from the response is
    /// dropped by the extension (`listener.ts` rebuilds the document's state array from
    /// `response.imports`), and a later push can only UPDATE a state, never create one
    /// (`refreshMerge.ts` maps over the states that exist). Omitting a still-building import
    /// would therefore lose it permanently.
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
        // Interactive size read → serve stale (SWR) unless the client forces fresh
        // (CI / CLI budget checks). A stale serve here triggers the background
        // revalidation + RefreshedResults push in the FileSizeDocument handler.
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
    /// The file's own totals still come from a real build — ONE combined build per runtime,
    /// bounded by `BUILD_TIMEOUT`, whose entries include the imports that are still being
    /// measured individually (their bytes belong in the file's total whether or not their own
    /// number has landed). What this no longer does is wait for those individual builds: their
    /// states come back `Loading`, and `AnalyzeDocument`'s streaming pass — which the extension
    /// always sends first, for the same document and the same generation — is what builds them
    /// and pushes each one to the client.
    ///
    /// A force-fresh request (CI) is served by the blocking path instead: completeness is the
    /// entire point of that flag.
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
        // EVERY detected import reaches the aggregate — including one with no `request`, because a
        // request carries the installed version and there is none.
        //
        // Such an import used to be `filter_map`ped away right here, so it never reached the floor
        // check: the file's total silently omitted it, was cached, was persisted as the file's
        // baseline, and `importlens check` exited 0 on a number that was missing a whole dependency.
        // It is a floor now, like every other unmeasured contributor (SRS FR-024a, bullet 4).
        //
        // But "no request" is TWO facts, and flagging both cost a regression of its own. A **path
        // alias** (`@app/components`, a bare `components/Button` under a `baseUrl`) is not a package
        // at all — it points at first-party source, which Import Lens does not measure (ADR-0004),
        // so its zero is a fact and not a gap. Treating an alias as a missing dependency made every
        // file that uses path aliases a permanent floor: never cached, never persisted, never judged.
        //
        // The discriminator is POSITIVE evidence — the specifier resolves, through tsconfig `paths`,
        // to first-party source — and never the absence of it. A specifier that resolves to nothing
        // is a floor, whether the project declared it or not: a typo and an uninstalled dependency
        // omit the same bytes from this total, and refusing a verdict is the direction ADR-0006
        // demands to fail in.
        //
        // ONE probe for the whole loop. It holds the workspace's alias resolvers — one per reachable
        // tsconfig, each a `Resolver::new` and a cold JSONC parse — and asking for them per specifier
        // made this `O(aliased imports × reachable configs)`: on the create-vue shape a 20-alias page
        // component burned ~20 ms of the 50 ms NFR-002 warm budget here, on every debounced
        // keystroke. Built once, reused across the loop, it is `O(reachable configs)`.
        //
        // It must not outlive this response, and it does not: a `Resolver` that survives the request
        // memoizes the filesystem, and the miss it memoizes is the one answer that must never be
        // cached — an import written before the file it points at would stay a floor for the daemon's
        // life, even after the developer created the file (`ResolverSet::alias_config_graphs`). The
        // probe also builds nothing until the first import that HAS no request, so a document whose
        // every import is installed still pays nothing.
        //
        // It is keyed on the WORKSPACE, not on the document: "does this specifier map to first-party
        // source?" is a question about the project's alias table, and the answer must not change
        // because the import happens to sit in a `.vue` file rather than a `.ts` one. It did — see
        // `FirstPartySourceProbe`.
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
            // The one fact the bytes cannot carry: whether every import that belongs in them was
            // really measured. The extension needs it to keep a floor out of its persisted
            // bundle-impact history (FR-026c) — a store with no TTL, where one fabricated row
            // becomes the file's permanent baseline.
            incomplete: file_size.incomplete,
            // And the fact `incomplete` cannot carry: whether the file's OWN combined build
            // succeeded. It can fail with every contributor Measured, and then these totals are an
            // un-deduplicated per-import sum — a different quantity, and an over-count.
            degraded: file_size.degraded,
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
        let analyzed_results = drain_ordered_owned(pending_analysis, |_, (index, pending)| {
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
                    .analyze_with_cache(&context, &import_request, false, ReadIntent::Interactive),
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
        // A dependency of a `package.json` has no document position and so no runtime split; its
        // request carries the runtime all the same, and taking it from there keeps ONE source of
        // the partition rather than assuming a default here.
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

        // The runtime is already classified from the LIVE editor buffer (`request.source`)
        // by `named_import_completion_context` — the one document classifier — so hand it
        // to the enumeration directly rather than re-deriving it from disk. `cursor_offset`
        // is therefore `None`: the runtime is not re-classified for this path.
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

    /// The enumeration itself, once the runtime has been decided. Both callers reach it
    /// with a runtime derived from the ONE document classifier — the completion popup from
    /// the live buffer, the direct request from the cursor offset — so a hardcoded
    /// `Component` (the old bug) cannot creep back in. The runtime drives BOTH resolution
    /// (`browser` vs `node` conditions pick a different entry file) and the memo key.
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
                // A successful enumeration's warnings used to be dropped here.
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
            crate::engine::dependency_paths::purge_missing();
        }

        // Drop the derived L1/graph caches when a store-clearing scope ran. `All`
        // clears them UNCONDITIONALLY (X-21): a "Clear everything" that removed no
        // shard (nothing was cached yet, or only the registry was populated) must
        // still drop the derived caches so no stale derived state survives. Scoped
        // shard removals still only pay this when they actually removed a shard;
        // the registry-only scope leaves these caches untouched.
        if matches!(request.scope, CacheRemoveScope::All) || !removed.is_empty() {
            crate::engine::dependency_paths::clear();
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
            crate::engine::dependency_paths::invalidate_package(package_name);
        }
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
        true
    }

    /// A `tsconfig.json` / `jsconfig.json` changed on disk, so the workspace's **alias table** did.
    ///
    /// That table is the sole discriminator between a path alias and a package that is not
    /// installed (`pipeline::resolver::FirstPartySourceProbe`), and the daemon read it
    /// exactly once: `oxc_resolver` memoizes the parsed config in the shared resolver's FS cache,
    /// and nothing ever dropped it. So a developer hitting the floor the SRS tells them to repair —
    /// "mirror the alias into tsconfig `paths`" — applied the repair, saved the file, and the
    /// daemon went on returning `incomplete: true` for the rest of its life. The remedy the spec
    /// prescribes did nothing.
    ///
    /// **What this still buys, now that the alias resolvers memoize no filesystem fact.** A `paths`
    /// edit no longer needs a message at all: the resolvers are rebuilt per query (which is what
    /// stops a floor being sticky), so the config is re-read on the next request. What survives the
    /// query is the **reachable-config walk** — which projects the workspace's `references` graph
    /// reaches — and a config that starts *referencing* the project that owns the `paths` is
    /// invisible until that memo is dropped. This is what drops it.
    ///
    /// It rides the SAME path a `node_modules` change already rides (the extension's watcher →
    /// `node_modules_changed` → here → `invalidate_shared_resolvers`), because it is the same fact:
    /// something the resolvers memoized is no longer true.
    ///
    /// What it does NOT do is bump the cache generation or touch a shard. A tsconfig has no bearing
    /// on what a package *weighs* — package entries are resolved by the runtime resolvers, which
    /// never read it — so re-verifying every cached import against disk would buy nothing. What it
    /// does invalidate is the L1 **aggregate**: a file whose alias classification flips changes
    /// which of its imports contribute bytes, and whether its total is a floor at all.
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

    /// Settle every import the daemon can answer **without an engine build**, and mark the rest
    /// `Loading`.
    ///
    /// Three outcomes, none of which can park:
    ///
    /// - a cache hit → `Ready` with its result;
    /// - an import that does not resolve to a package at all → settled here, because that path
    ///   never reaches the engine: `analyze_import` falls through to the manifest approximation,
    ///   the declaration-only result, or a typed error, all of which are filesystem work;
    /// - a real miss → `Loading`, with the resolved package and cache key carried out in
    ///   [`StreamedDocumentAnalysis::pending`] so the caller can build it off the response path.
    ///
    /// The `Loading` item keeps its `request`, so a caller that only needs the resolved package
    /// identity (the extension's named-export candidates command) is unaffected by the fact that
    /// its size has not landed.
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
                    CacheProbe::Unresolved => {
                        let result = analyze_import(context, &request);
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
    /// the moment it lands. Runs off the response path (the IPC server spawns it), so a build
    /// that parks for the full `BUILD_TIMEOUT` delays nothing but its own import.
    ///
    /// `should_continue` is checked before each build: a newer analysis of the same document
    /// supersedes this one, and finishing builds for a document state the user has already
    /// edited past buys nobody anything. Results still go through `analyze_and_cache`, so the
    /// single-flight registry collapses a build another request is already running for the same
    /// key rather than starting a second one.
    ///
    /// **Shared bytes close the document, not each import.** `shared_bytes` says how much of an
    /// import's weight another import in the SAME file also pulls in, so it is not knowable until
    /// every import of the file has been measured — and on a cold document that is only true once
    /// the last push has landed. Each import is therefore delivered the moment it is measured (its
    /// own number is what the user is waiting for), and one final push carries the imports whose
    /// shared-byte figure the client does not yet have right. Without it, the shared-dependency
    /// insight would silently never appear on a first analysis, because `annotate_ready_items` can
    /// only annotate imports that already have a result and a cold document has none.
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

        // A superseded document is not worth a closing pass: the client has already dropped
        // everything this stream pushed it.
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
        // Same split as the batch handlers: an import the cache can answer — or one
        // that does not resolve at all — is settled at pool width; only a real miss
        // queues for an engine permit. This path serves both interactive document
        // analysis and every file of a workspace report.
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
                    CacheProbe::Hit(result) => Ok(ImportAnalysisItem {
                        result: Some(*result),
                        detected: detected.clone(),
                        status: ImportAnalysisStatus::Ready,
                        message: None,
                        request: Some(request),
                    }),
                    pending => Err((request, pending)),
                }
            },
            |_, detected, (request, pending)| ImportAnalysisItem {
                result: Some(self.complete_probe(context, &request, pending)),
                detected: detected.clone(),
                status: ImportAnalysisStatus::Ready,
                message: None,
                request: Some(request),
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
        // Offered unconditionally: `FileSizeCache::insert` refuses a total that is not a
        // measurement of the file (a floor, or one a parked combined build degraded). The gate is
        // the store's, so it cannot be forgotten here or at the next call site added.
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
        let probe = self.probe_cache(context, request, serve_stale, intent);
        self.complete_probe(context, request, probe)
    }

    /// The lookup half of `analyze_with_cache`, with the build half left undone.
    ///
    /// Splitting the two is what lets a batch classify every import pool-wide and
    /// then feed only the misses to the two-permit engine drain. §9 bounds *builds*
    /// at two; it says nothing about cache hits, and serving those two-at-a-time
    /// throttled the overwhelmingly common case to the width of the rarest one.
    ///
    /// A miss carries its resolved package and cache key forward so `complete_probe`
    /// does not resolve the manifest a second time.
    fn probe_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        serve_stale: bool,
        intent: ReadIntent,
    ) -> CacheProbe {
        let Ok(resolved) = resolve_package_entry(&context.active_document_path, request) else {
            return CacheProbe::Unresolved;
        };
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
            // Force-fresh (CI / `importlens check`, §4.5): serve ONLY a value verified
            // `Fresh` against disk, across BOTH the memory working set and the disk
            // cache. `get_if_fresh` returns `None` on Unknown/Stale/Gone/miss, so a
            // transient `Unknown` — which the evicting `get` would launder into a
            // `cache_hit`, whether memory-resident OR cold-daemon disk-hydrated — never
            // reaches CI; we recompute synchronously below instead. This single gate
            // also removes the double dependency re-verification of the prior
            // memory-only `probe_freshness` + `get`.
            if let Some(result) = fresh_cached_result_for_key(cache.as_ref(), request, &key, intent)
            {
                return CacheProbe::Hit(Box::new(result));
            }
        }

        CacheProbe::Miss(Box::new(PendingBuild { resolved, key }))
    }

    /// The build half. Only this may occupy an engine permit.
    fn complete_probe(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        probe: CacheProbe,
    ) -> ImportResult {
        match probe {
            CacheProbe::Hit(result) => *result,
            CacheProbe::Unresolved => analyze_import(context, request),
            CacheProbe::Miss(pending) => {
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
        }
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

/// The runtime each request resolves under, in request order — the partition
/// [`annotate_shared_bytes`] counts sharing within (ADR-0005). The batch handlers' results are
/// index-aligned with their requests, which is the same alignment `SizedImport::installed` already
/// relies on in `handle_file_size`.
fn runtimes_of(requests: &[ImportRequest]) -> impl Iterator<Item = ImportRuntime> + '_ {
    requests.iter().map(|request| request.runtime)
}

/// Re-derive `shared_bytes` across a document's COMPLETE set of measurements and return only the
/// imports whose figure the client does not already hold correctly.
///
/// Everything the client has was annotated against a PARTIAL set: an import answered from cache was
/// annotated against the response's cache hits alone (`annotate_ready_items`), and an import that
/// streamed in carried whatever its own build produced, which is no annotation at all. Neither can
/// be right until the last import of the file has been measured — sharing is a relation between two
/// imports of the same document.
///
/// A document with nothing shared produces nothing: an import whose shared bytes are zero reads the
/// same to the client whether the field is `Some(0)` or absent (`insights.ts` and the tooltip both
/// gate on `> 0`), so re-sending it would be a frame that changes nothing on screen.
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
/// Imports still being measured contribute nothing: shared bytes are computed from module
/// contributions, and an import with no result has none. `complete_pending_imports` re-derives the
/// figure over the whole document once the last streamed import has landed, and pushes the
/// corrections (`shared_bytes_corrections`).
fn annotate_ready_items(items: &mut [ImportAnalysisItem]) {
    annotate_shared_bytes(items.iter_mut().filter_map(|item| {
        // Read the runtime off the item BEFORE the result is borrowed mutably; it is the same
        // `DetectedImport` runtime that decides which combined build the import is sized in.
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
            // Nothing was summed at all; `error` is the answer, and every client already refuses
            // an errored response.
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
/// A **pre-check**, not the gate. The gate is `ImportResult::is_durable`, and it lives inside the
/// stores themselves (`ImportCache::insert*`, `DiskCache::insert*`) — because a predicate a caller
/// must remember to call is exactly the shape of every defect this model exists to end. This
/// function is here to spare the work a refused insert would waste (the dependency fingerprints),
/// and it asks the store's own question so the two can never disagree.
///
/// What is cached: a Measured result, and an Unmeasured one whose stage is a property of the
/// package's **bytes** (`parse`, `link`, `oversized_entry`, an unreadable manifest, an unresolvable
/// entry). The cache is keyed by those bytes' fingerprints, so such a fact expires exactly when it
/// would change — and refusing it would re-enter the engine for a broken package on *every*
/// analysis, forever, on one of only two permits.
///
/// What is not: a transient outcome, an IO condition (`entry_metadata`), and any stage nobody has
/// classified. See `pipeline::stage::may_enter_a_durable_store`.
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
        // The COPY. `DetectedImport.runtime` is the one source of a document's runtime split
        // (`document::script_regions`), and this line is the one derivation that carries it onto the
        // request `pipeline::file_size` groups its builds by. Break it — a constant here — and an
        // Astro file's Server and Client imports collapse into one bundle, `shared-core` is linked
        // once for two payloads that each ship it, and the compressed total under-reports by ~49%
        // with nothing failing (ADR-0005). Pinned by
        // `tests/file_size_runtime.rs::a_mixed_runtime_astro_document_is_built_as_two_artifacts`.
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
        // The engine captured a fingerprint as it read each module, so the stored hash
        // describes the exact bytes the size was measured from. Only the manifest and
        // any binary module the plugin did not read need hashing here.
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
        // Static fallback: no graph was built, so there is nothing that was measured
        // for these to be inconsistent with.
        None => vec![
            resolved.package_root.join("package.json"),
            resolved.entry_path.clone(),
        ]
        .into_iter()
        .filter_map(file_fingerprint_reading_hash)
        .collect(),
    };

    // Two ids can canonicalize to the same real path (a symlinked workspace dep), so
    // dedup is load-bearing, not cosmetic.
    sort_and_dedup_fingerprints(&mut fingerprints);
    fingerprints
}
/// Lives here rather than in `ipc::server` because the streaming document handler builds one
/// itself: a protocol error is a settled analysis with nothing left to build.
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
        incomplete: false,
        degraded: false,
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic::for_stage("protocol", message)],
    }
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

/// The runtime a direct `enumerate_exports` request resolves under, from the ONE document
/// classifier (`document::runtime_at_offset`) so it cannot disagree with the size path.
///
/// The request carries the cursor's UTF-16 offset when the caller has one; the daemon owns
/// the classification (ADR-0002), reading the document from disk. Absent — a plain file, or
/// an older client that never sent it — or unreadable, the answer is `Component`, which is
/// the correct default for a document with no runtime-bearing regions.
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
        // Nothing was summed at all; `error` is the answer.
        incomplete: false,
        degraded: false,
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: crate::pipeline::stage::PROTOCOL.to_owned(),
            message,
            details: Vec::new(),
        }],
    }
}

fn protocol_error(request: &ImportRequest, message: String) -> ImportResult {
    let mut result = ImportResult::unmeasured(
        request.specifier.clone(),
        crate::pipeline::stage::PROTOCOL,
        message,
        vec![format!("specifier: {}", request.specifier)],
    );
    result.confidence_reasons =
        vec!["Protocol validation failed before a bundle size could be measured.".to_owned()];
    result
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
        // The follower re-reads the process-global cache generation inside `analyze_and_cache`. A
        // sibling test that bumps it in between would make the follower its own leader, and this
        // test would fail for a reason that has nothing to do with single-flight.
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
            cache.get(&key).is_some(),
            "a follower with should_store=true must keep its cache write even when the leader did not store",
        );
    }
}

/// **Property** over every durable store the daemon writes, quantified over **every** stage the
/// daemon can produce that is not a property of the package's bytes.
///
/// It quantifies over the STORES: it hands each one a real result and then asks the store what it
/// kept. The previous version quantified over two *predicates* (`should_cache_result`,
/// `FileSizeComputation::is_cacheable`) — and the stores themselves had no gate at all:
/// `ImportCache::insert`, `DiskCache::insert_at_generation` and `FileSizeCache::insert` took
/// anything they were given. It proved that two functions returned `false`, not that a transient
/// result could not be written down, and the next caller who forgot to consult them would have
/// written one with nothing failing. The gate now lives in each store; this proves it is there.
///
/// The daemon's four *build-derived* stores — `pipeline::full_package`, `pipeline::export_list`,
/// `pipeline::build_memo` and `engine::dependency_paths` — are absent on purpose: none of them can
/// be handed an `ImportResult` at all. Their only input is a `BundleArtifact` / `ExportEnumeration`,
/// which exists solely on the `Ok` side of a build, so a failure of any kind is unrepresentable
/// there rather than merely refused. `scripts/test/result-model-guards.test.mjs` fails if anyone
/// plumbs a result into one of them.
///
/// The extension's two persisted histories (`workspaceState` / `globalState`) are the same property
/// in TypeScript: `extension/test/analysis/transience.test.ts`.
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

    /// Every stage a durable store must REFUSE: request-local engine outcomes plus machine-local
    /// pipeline work (`entry_metadata` and `compression`). DERIVED from the allowlist rather than
    /// restated beside it, so a stage that changes classification changes this list with it.
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

    /// The L1 file-size aggregate, built the way the real fallback builds it — from per-import
    /// results — rather than hand-assembled. A hand-assembled `FileSizeComputation` cannot see the
    /// defect ADR-0006 invariant 4 names, because the bug is in how a result is *turned into* a
    /// total.
    fn file_total(results: Vec<(&str, ImportResult)>) -> FileSizeComputation {
        let sized = results
            .into_iter()
            .map(|(specifier, result)| SizedImport::installed(request(specifier), Some(result)))
            .collect::<Vec<_>>();
        per_import_totals_for_test(&sized)
    }

    /// **The L1 import cache.** Not "the predicate the caller should have used" — the store.
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
                cache.get(&key).is_none(),
                "`{stage}` says nothing about the package's bytes; the L1 store must keep nothing"
            );
        }
    }

    /// **The L1 import cache, the other transient shape**: a build that SUCCEEDED, whose
    /// full-package comparison build then failed transiently. Its sizes are real; its
    /// `truly_treeshakeable: false` is fabricated by the same accident, and caching it marks a
    /// healthy package "not tree-shakeable" for a whole cache generation.
    ///
    /// This shape is REPRESENTABLE — a real state, deliberately kept — so the STORE is what must
    /// refuse it.
    #[test]
    fn the_l1_import_cache_refuses_a_measurement_whose_comparison_build_degraded_transiently() {
        for stage in stage::ALL
            .iter()
            .copied()
            .filter(|candidate| stage::is_transient(candidate))
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
                cache.get(&key).is_none(),
                "`{stage}`: real sizes, but a tree-shaking verdict that is a scheduling accident"
            );
        }
    }

    /// **The L2 disk cache.** A store in its own right, and the worst one to poison: it outlives the
    /// process. Gated independently of the L1 cache in front of it, so "the caller already checked"
    /// is load-bearing nowhere.
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
                disk.get(&key).is_none(),
                "`{stage}`: L2 outlives the process; a scheduling accident must not"
            );
        }

        // Control: the store is not simply broken. A deterministic failure IS persisted — it is a
        // property of the package's bytes, and the entry expires with them.
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
            disk.get("v4:broken-lib:parse").is_some(),
            "a deterministic failure is a fact about the package and IS persisted (invariant 3)"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **The L1 file-size aggregate.** Quantified over every non-durable stage, over every DURABLE
    /// one, and over the state no stage describes at all — an import still being measured — because
    /// invariant 4 is about the total's INPUTS, not about anything having failed.
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

        // **The seventh instance.** A DETERMINISTIC failure is cached as a per-import fact
        // (invariant 3) and STILL makes the file's total a floor (invariant 4). The two invariants
        // are about different things, and conflating them is what this test was blind to: the total
        // was cached, persisted as the file's permanent baseline, and passed by CI with exit 0.
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

        // And the state no stage describes: an import whose own build has not landed yet.
        let cache = FileSizeCache::new();
        let loading = per_import_totals_for_test(&[
            SizedImport::installed(request("alpha"), Some(measured("alpha", 100))),
            SizedImport::installed(request("beta"), None),
        ]);
        assert!(loading.incomplete);
        cache.insert(path.clone(), 1, loading);
        assert!(cache.get(&path, 1).is_none());

        // And the shape `incomplete` structurally cannot see (ADR-0006, invariant 4, second half):
        // every contributor Measured, `error: None`, a real number — and the file's OWN combined
        // build failed, so that number is an un-deduplicated per-import sum, not a File Cost.
        // `file_size.rs::a_failed_combined_build_degrades_the_total_even_with_every_import_measured`
        // proves the flag is raised; this proves the STORE refuses it.
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

        // Control: every import measured — this really IS the file, and it must still cache.
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

    /// The other half, and the owner's decision: a DETERMINISTIC per-import outcome IS cached, sizes
    /// or no sizes. It is a property of the package's bytes, the cache is keyed by those bytes'
    /// fingerprints, and refusing it would re-enter the engine for a broken package on every
    /// analysis, forever, on one of only two permits.
    #[test]
    fn the_l1_import_cache_still_keeps_every_deterministic_outcome() {
        for stage in durable_failure_stages() {
            let cache = ImportCache::new(None, false);
            let key = format!("v4:broken-lib:{stage}");
            let result =
                ImportResult::unmeasured("broken-lib", stage, "no matching export", vec![]);

            cache.insert_with_fingerprints(key.clone(), result, Vec::<FileFingerprint>::new());
            assert!(
                cache.get(&key).is_some(),
                "`{stage}` will happen again next time; withholding it buys a rebuild and no \
                 correctness"
            );
        }
    }
}
