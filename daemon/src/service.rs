use crate::{
    cache::{
        key::{cache_key_for_resolved_import, fingerprints_for_paths},
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
        BatchRequest, BatchResponse, CacheCleanupRequest, CacheCleanupResponse, CacheListRequest,
        CacheListResponse, CacheRemoveRequest, CacheRemoveResponse, CacheRemoveScope,
        CacheStatusRequest, CacheStatusResponse, CompleteImportMembersRequest,
        CompleteImportMembersResponse, ConfidenceLevel, DetectedImport, EnumerateExportsRequest,
        EnumerateExportsResponse, FileSizeDocumentRequest, FileSizeDocumentResponse,
        FileSizeRequest, FileSizeResponse, ImportAnalysisItem, ImportAnalysisStatus,
        ImportDiagnostic, ImportKind, ImportRequest, ImportResult, ImportRuntime, ImportSyntax,
        PROTOCOL_VERSION, PackageJsonDependencyAnalysisItem,
        RegistryHintMode as ProtocolRegistryHintMode, RegistryHintResult, RegistryHintTarget,
        WorkspaceReportRequest, WorkspaceReportResponse, WorkspaceReportSummary,
        is_supported_protocol_version,
    },
    pipeline::analyze::{AnalysisContext, analyze_import, analyze_resolved_import},
    pipeline::file_size::{annotate_shared_bytes, compute_file_size},
    pipeline::graph::{
        ModuleGraph, ModuleId, build_module_graph_cached, build_module_graph_cached_with_runtime,
        clear_module_graph_cache, invalidate_module_graph_cache_for_package,
    },
    pipeline::resolver::{ResolvedPackage, find_package_root, resolve_package_entry},
};
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

// `RegistryHintService` and `RegistryRefreshExecutor` hold trait objects and a
// thread pool respectively, so `ImportLensService` no longer derives `Debug`.
pub struct ImportLensService {
    cache_registry: ProjectCacheRegistry,
    registry_hints: crate::registry::service::RegistryHintService,
    registry_executor: crate::registry::executor::RegistryRefreshExecutor,
    report_executor: crate::report::executor::WorkspaceReportExecutor,
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
        Self::new_with_cache_policy(storage_path, enable_disk_cache, 512, 30)
    }

    pub fn new_with_cache_policy(
        storage_path: Option<PathBuf>,
        enable_disk_cache: bool,
        cache_max_size_mb: u64,
        cache_max_age_days: u64,
    ) -> Self {
        let cache_registry = ProjectCacheRegistry::new(
            storage_path.clone(),
            enable_disk_cache,
            cache_max_size_mb,
            cache_max_age_days,
        );
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
            registry_hints,
            registry_executor,
            report_executor,
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
            cache_registry: ProjectCacheRegistry::new(None, false, 512, 30),
            registry_hints,
            registry_executor: crate::registry::executor::RegistryRefreshExecutor::new(
                crate::registry::constants::REGISTRY_REFRESH_CONCURRENCY,
            ),
            report_executor: crate::report::executor::WorkspaceReportExecutor::new(),
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
        cache_max_age_days: u64,
    ) -> Self {
        Self {
            cache_registry: ProjectCacheRegistry::new(
                storage_path,
                enable_disk_cache,
                cache_max_size_mb,
                cache_max_age_days,
            ),
            registry_hints: self.registry_hints,
            registry_executor: self.registry_executor,
            report_executor: self.report_executor,
            preserve_registry_across_hello: self.preserve_registry_across_hello,
        }
    }

    pub fn preserve_registry_across_hello(&self) -> bool {
        self.preserve_registry_across_hello
    }

    pub fn refresh_registry_hint_target(
        &self,
        target: RegistryHintTarget,
        mode: ProtocolRegistryHintMode,
        now_ms: u64,
    ) -> RegistryHintResult {
        let service_mode = match mode {
            ProtocolRegistryHintMode::RefreshStale => {
                crate::registry::service::RegistryHintMode::RefreshStale
            }
            ProtocolRegistryHintMode::ForceRefresh => {
                crate::registry::service::RegistryHintMode::ForceRefresh
            }
            ProtocolRegistryHintMode::Off | ProtocolRegistryHintMode::Cached => {
                crate::registry::service::RegistryHintMode::Cached
            }
        };

        let lookup = self.registry_hints.hint_for(
            &target.name,
            target.installed_version.as_deref(),
            service_mode,
            now_ms,
        );

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

    pub fn spawn_registry_refresh(&self, job: impl FnOnce() + Send + 'static) {
        self.registry_executor.spawn(job);
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

        self.build_workspace_report_inner(request)
    }

    fn build_workspace_report_inner(
        &self,
        request: WorkspaceReportRequest,
    ) -> WorkspaceReportResponse {
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

            self.handle_analyze_document(document_request, ignore_resolver)
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
            .map(|item| self.analyze_with_cache(&context, item))
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
                let result = self.analyze_with_cache(&context, item);
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
            .map(|item| self.analyze_with_cache(&context, item))
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
        let imports = self.analysis_items_for_detected(&context, detected);

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
        let imports = self.analysis_items_for_detected(&context, detected);

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
        let states = self.analysis_items_for_detected(&context, detected);
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
        // Resolve each dependency's installed version (an ancestor walk plus a
        // package.json read) in parallel; into_par_iter preserves order, so the
        // resulting states and import_requests still line up with streaming
        // indexes exactly as the sequential loop did.
        type PreparedDependency = (ImportRequest, Option<ResolvedPackage>);
        let resolved: Vec<(
            PackageJsonDependencyAnalysisItem,
            Option<PreparedDependency>,
        )> = package_json_dependency_entries(&request.source)
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
        }

        let indexed_results = import_requests
            .par_iter()
            .enumerate()
            .filter_map(|(index, prepared)| prepared.as_ref().map(|item| (index, item)))
            .map(|(index, (import_request, resolved))| {
                let result = match resolved {
                    Some(resolved) => {
                        self.analyze_resolved_with_cache(&context, import_request, resolved.clone())
                    }
                    None => self.analyze_with_cache(&context, import_request),
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
        let (indexes, mut results): (Vec<_>, Vec<_>) = indexed_results.into_iter().unzip();
        annotate_shared_bytes(&mut results);
        for (index, result) in indexes.into_iter().zip(results) {
            states[index].status = ImportAnalysisStatus::Ready;
            states[index].result = Some(result);
        }

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

        let mut exports = enumerate_graph_exports(&graph);
        exports.sort();
        exports.dedup();

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
                max_age_days: 0,
                last_cleanup_millis: None,
                current_project: None,
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
            max_age_days: status.max_age_days,
            last_cleanup_millis: status.last_cleanup_millis,
            current_project: status.current_project,
            error: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn cleanup_cache(&self, request: CacheCleanupRequest) -> CacheCleanupResponse {
        if !is_supported_protocol_version(request.version) {
            return CacheCleanupResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                total_size_bytes: 0,
                removed: Vec::new(),
                failed: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: vec![ImportDiagnostic::for_stage(
                    "protocol",
                    "unsupported protocol version",
                )],
            };
        }

        let cleanup = self.cache_registry.cleanup();

        if !cleanup.removed.is_empty() {
            clear_module_graph_cache();
        }

        CacheCleanupResponse {
            version: request.version,
            request_id: request.request_id,
            total_size_bytes: cleanup.total_size_bytes,
            removed: cleanup.removed,
            failed: cleanup.failed,
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
            CacheRemoveScope::All => self.cache_registry.remove_all(),
            CacheRemoveScope::Orphans => {
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
        }

        if !removed.is_empty() {
            clear_module_graph_cache();
            // Drop L1 aggregate sizes too so the status-bar size recomputes fresh
            // after a cache clear (the memory-only L1 is not generation-bumped here).
            crate::pipeline::file_size_cache::shared_file_size_cache().clear();
        }

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
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
    }

    pub fn invalidate_all(&self) {
        self.cache_registry.clear_all();
        clear_module_graph_cache();
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
    }

    pub fn cache_len(&self) -> usize {
        self.cache_registry.memory_len()
    }

    pub fn recent_cache_keys(&self, workspace_root: &Path, limit: usize) -> Vec<String> {
        self.cache_registry.recent_keys(workspace_root, limit)
    }

    pub fn flush_cache(&self) -> Result<(), String> {
        self.cache_registry.flush_to_disk()
    }

    pub fn flush_cache_recency_touches(&self) {
        self.cache_registry.flush_recency_touches();
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

        if cache.get(&key).is_some() || !should_continue() {
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
            let Some(package_name) = package_name_from_package_json_path(package_json_path) else {
                // A path we can't map to a package name is opaque, so fall back
                // to a full invalidation -- the only safe option.
                self.invalidate_all();
                return true;
            };
            package_names.push(package_name);
        }

        if package_names.is_empty() {
            return false;
        }

        // Even for a large burst, invalidate only the affected packages (a single
        // decode pass via `invalidate_packages`) rather than nuking every project
        // shard under this workspace's cache base -- a full clear would evict
        // unrelated sibling projects in a multi-root / monorepo window. The
        // graph/resolver/generation invalidations run once for the whole burst.
        self.cache_registry.invalidate_packages(&package_names);
        for package_name in &package_names {
            invalidate_module_graph_cache_for_package(package_name);
        }
        crate::pipeline::resolver::invalidate_shared_resolvers();
        crate::cache::memory::bump_cache_generation();
        true
    }

    fn analysis_items_for_detected(
        &self,
        context: &AnalysisContext,
        detected: Vec<DetectedImport>,
    ) -> Vec<ImportAnalysisItem> {
        let mut items = detected
            .into_par_iter()
            .map(|detected| {
                match import_request_for_detected(&context.active_document_path, &detected) {
                    Ok(request) => ImportAnalysisItem {
                        result: Some(self.analyze_with_cache(context, &request)),
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
    ) -> ImportResult {
        match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => self.analyze_resolved_with_cache(context, request, resolved),
            Err(_) => analyze_import(context, request),
        }
    }

    // Cache lookup + analysis for an already-resolved package. The package.json
    // analysis path resolves each dependency once and reuses the ResolvedPackage
    // here, avoiding a second resolve_package_entry (and its manifest read).
    fn analyze_resolved_with_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        resolved: ResolvedPackage,
    ) -> ImportResult {
        let key = cache_key_for_resolved_import(request, &resolved);
        let cache = self.cache_registry.cache_for_root(&context.workspace_root);

        if let Some(result) = cache.get(&key) {
            return result;
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
        let result = analyze_resolved_import(context, request, resolved.clone());

        if should_cache_result(&result) && should_store() {
            let fingerprints = dependency_fingerprints(request, &resolved, &result);
            self.cache_full_variant_alias(cache, request, &result, &resolved, &fingerprints);
            cache.insert_with_fingerprints(key, result.clone(), fingerprints);
        }

        result
    }

    fn cache_full_variant_alias(
        &self,
        cache: &ImportCache,
        request: &ImportRequest,
        result: &ImportResult,
        resolved: &ResolvedPackage,
        dependency_fingerprints: &[crate::cache::key::FileFingerprint],
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
        cache.insert_with_fingerprints(
            namespace_key,
            namespace_result,
            dependency_fingerprints.to_vec(),
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
    request: &ImportRequest,
    resolved: &ResolvedPackage,
    result: &ImportResult,
) -> Vec<crate::cache::key::FileFingerprint> {
    let mut paths = vec![
        resolved.package_root.join("package.json"),
        resolved.entry_path.clone(),
    ];

    if !result.is_cjs
        && let Ok(graph) =
            build_module_graph_cached_with_runtime(&resolved.entry_path, request.runtime)
    {
        paths.extend(graph.modules.iter().map(|module| module.path.clone()));
        paths.extend(graph.dependency_paths.iter().cloned());
    }

    fingerprints_for_paths(paths)
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

fn enumerate_graph_exports(graph: &ModuleGraph) -> Vec<String> {
    let mut exports = Vec::new();
    collect_module_exports(
        graph,
        graph.entry_id,
        true,
        &mut HashSet::new(),
        &mut exports,
    );
    exports
}

fn collect_module_exports(
    graph: &ModuleGraph,
    module_id: ModuleId,
    include_default: bool,
    visited: &mut HashSet<ModuleId>,
    exports: &mut Vec<String>,
) {
    if !visited.insert(module_id) {
        return;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return;
    };

    exports.extend(
        module
            .exports
            .iter()
            .filter(|export| include_default || export.exported_name != "default")
            .map(|export| export.exported_name.clone()),
    );
    exports.extend(
        module
            .reexports
            .iter()
            .filter(|reexport| include_default || reexport.exported_name != "default")
            .map(|reexport| reexport.exported_name.clone()),
    );

    for star_export in &module.star_exports {
        if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path) {
            collect_module_exports(graph, target_id, false, visited, exports);
        }
    }
}

fn protocol_error(request: &ImportRequest, message: String) -> ImportResult {
    ImportResult {
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
