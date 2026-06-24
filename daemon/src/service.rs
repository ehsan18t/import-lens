use crate::{
    cache::{
        key::{cache_key_for_resolved_import, fingerprints_for_paths},
        memory::ImportCache,
    },
    document::{
        analyze_imports, get_package_name, is_runtime_package_specifier, load_import_lens_ignore,
        named_import_completion_context, package_json_dependency_entries,
        package_json_dependency_sections, should_ignore_import,
    },
    ipc::protocol::{
        AnalyzeDocumentRequest, AnalyzeDocumentResponse, AnalyzePackageJsonRequest,
        AnalyzePackageJsonResponse, AnalyzeSpecifiersRequest, AnalyzeSpecifiersResponse,
        BatchRequest, BatchResponse, CompleteImportMembersRequest, CompleteImportMembersResponse,
        ConfidenceLevel, DetectedImport, EnumerateExportsRequest, EnumerateExportsResponse,
        FileSizeDocumentRequest, FileSizeDocumentResponse, FileSizeRequest, FileSizeResponse,
        ImportAnalysisItem, ImportAnalysisStatus, ImportDiagnostic, ImportKind, ImportRequest,
        ImportResult, ImportRuntime, ImportSyntax, PROTOCOL_VERSION,
        PackageJsonDependencyAnalysisItem,
    },
    pipeline::analyze::{AnalysisContext, analyze_import, analyze_resolved_import},
    pipeline::file_size::{annotate_shared_bytes, compute_file_size},
    pipeline::graph::{
        ModuleGraph, ModuleId, build_module_graph_cached, build_module_graph_cached_with_runtime,
        clear_module_graph_cache, invalidate_module_graph_cache_for_package,
    },
    pipeline::resolver::{ResolvedPackage, find_package_root, resolve_package_entry},
    registry::RegistryHintStore,
};
use rayon::prelude::*;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct ImportLensService {
    cache: ImportCache,
    registry_hints: RegistryHintStore,
}

impl ImportLensService {
    pub fn new(storage_path: Option<PathBuf>, enable_disk_cache: bool) -> Self {
        Self {
            cache: ImportCache::new(storage_path.clone(), enable_disk_cache),
            registry_hints: RegistryHintStore::new(storage_path),
        }
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
        let file_size = compute_file_size(&context, &request.imports);

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
    ) -> AnalyzeDocumentResponse {
        if !is_supported_protocol_version(request.version) {
            return AnalyzeDocumentResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                imports: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: protocol_diagnostics("protocol", "unsupported protocol version"),
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
        ) {
            Ok(imports) => imports,
            Err(error) => {
                return AnalyzeDocumentResponse {
                    version: request.version,
                    request_id: request.request_id,
                    imports: Vec::new(),
                    error: Some(error.clone()),
                    diagnostics: protocol_diagnostics("document_parse", &error),
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
                diagnostics: protocol_diagnostics("protocol", "unsupported protocol version"),
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
                diagnostics: protocol_diagnostics("protocol", "unsupported protocol version"),
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
                    diagnostics: protocol_diagnostics("document_parse", &error),
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
        let file_size = compute_file_size(&context, &requests);

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
        if !is_supported_protocol_version(request.version) {
            return AnalyzePackageJsonResponse {
                version: request.version.min(PROTOCOL_VERSION),
                request_id: request.request_id,
                sections: Vec::new(),
                states: Vec::new(),
                error: Some(format!("unsupported protocol version {}", request.version)),
                diagnostics: protocol_diagnostics("protocol", "unsupported protocol version"),
            };
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let sections = package_json_dependency_sections(&request.source);
        let mut states = Vec::new();

        for entry in package_json_dependency_entries(&request.source) {
            let force_registry_refresh = request.force_registry_refresh
                && request
                    .refresh_section
                    .as_ref()
                    .is_none_or(|section| section == &entry.section);
            let resolution =
                resolve_installed_package_version(&context.active_document_path, &entry.name);

            match resolution {
                Ok(version) => {
                    let import_request = ImportRequest {
                        specifier: entry.name.clone(),
                        package_name: entry.name.clone(),
                        version: version.clone(),
                        named: Vec::new(),
                        import_kind: ImportKind::Namespace,
                        runtime: ImportRuntime::Component,
                    };
                    let result = self.analyze_with_cache(&context, &import_request);
                    let registry_hint = request
                        .include_registry_hints
                        .then(|| {
                            self.registry_hints.hint_for_package(
                                &entry.name,
                                Some(&version),
                                force_registry_refresh,
                            )
                        })
                        .flatten();

                    states.push(PackageJsonDependencyAnalysisItem {
                        name: entry.name.clone(),
                        section: entry.section.clone(),
                        entry,
                        status: ImportAnalysisStatus::Ready,
                        installed_version: Some(version),
                        registry_hint,
                        message: None,
                        result: Some(result),
                    });
                }
                Err(message) => {
                    let registry_hint = request
                        .include_registry_hints
                        .then(|| {
                            self.registry_hints.hint_for_package(
                                &entry.name,
                                None,
                                force_registry_refresh,
                            )
                        })
                        .flatten();

                    states.push(PackageJsonDependencyAnalysisItem {
                        name: entry.name.clone(),
                        section: entry.section.clone(),
                        entry,
                        status: ImportAnalysisStatus::Missing,
                        installed_version: None,
                        registry_hint,
                        message: Some(message),
                        result: None,
                    });
                }
            }
        }

        let mut results = states
            .iter_mut()
            .filter_map(|state| state.result.take())
            .collect::<Vec<_>>();
        annotate_shared_bytes(&mut results);
        let mut result_iter = results.into_iter();
        for state in &mut states {
            if state.status == ImportAnalysisStatus::Ready {
                state.result = result_iter.next();
            }
        }

        AnalyzePackageJsonResponse {
            version: request.version,
            request_id: request.request_id,
            sections,
            states,
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
                diagnostics: protocol_diagnostics("protocol", "unsupported protocol version"),
            };
        }

        let Some(context) = named_import_completion_context(&request.source, request.cursor_offset)
        else {
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
                    diagnostics: protocol_diagnostics("package_resolution", &error),
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

    pub fn invalidate_package(&self, package_name: &str) {
        self.cache.invalidate_package(package_name);
        invalidate_module_graph_cache_for_package(package_name);
    }

    pub fn invalidate_all(&self) {
        self.cache.clear();
        clear_module_graph_cache();
    }

    pub fn cache_len(&self) -> usize {
        self.cache.memory_len()
    }

    pub fn recent_cache_keys(&self, limit: usize) -> Vec<String> {
        self.cache.recent_keys(limit)
    }

    pub fn flush_cache(&self) -> Result<(), String> {
        self.cache.flush_to_disk()
    }

    pub fn prewarm_import<F>(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        should_continue: F,
    ) where
        F: Fn() -> bool,
    {
        let resolved = match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => resolved,
            Err(_) => return,
        };
        let key = cache_key_for_resolved_import(request, &resolved);

        if self.cache.get(&key).is_some() || !should_continue() {
            return;
        }

        let result = analyze_resolved_import(context, request, resolved.clone());

        if should_cache_result(&result) && should_continue() {
            let fingerprints = dependency_fingerprints(request, &resolved, &result);
            self.cache_full_variant_alias(request, &result, &resolved, &fingerprints);
            self.cache
                .insert_with_fingerprints(key, result, fingerprints);
        }
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

        if self.cache.get(&key).is_some() || !should_continue() {
            return;
        }

        let result = analyze_resolved_import(context, request, resolved.clone());

        if should_cache_result(&result) && should_continue() {
            let fingerprints = dependency_fingerprints(request, &resolved, &result);
            self.cache_full_variant_alias(request, &result, &resolved, &fingerprints);
            self.cache
                .insert_with_fingerprints(key, result, fingerprints);
        }
    }

    pub fn invalidate_package_json_paths(&self, package_json_paths: &[String]) -> bool {
        let mut invalidated_any = false;

        for package_json_path in package_json_paths {
            let Some(package_name) = package_name_from_package_json_path(package_json_path) else {
                self.invalidate_all();
                return true;
            };

            self.invalidate_package(&package_name);
            invalidated_any = true;
        }

        invalidated_any
    }

    fn analysis_items_for_detected(
        &self,
        context: &AnalysisContext,
        detected: Vec<DetectedImport>,
    ) -> Vec<ImportAnalysisItem> {
        let mut items = detected
            .par_iter()
            .map(|detected| {
                match import_request_for_detected(&context.active_document_path, detected) {
                    Ok(request) => ImportAnalysisItem {
                        detected: detected.clone(),
                        status: ImportAnalysisStatus::Ready,
                        message: None,
                        request: Some(request.clone()),
                        result: Some(self.analyze_with_cache(context, &request)),
                    },
                    Err(message) => ImportAnalysisItem {
                        detected: detected.clone(),
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

    fn analyze_with_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
    ) -> ImportResult {
        let resolved = match resolve_package_entry(&context.active_document_path, request) {
            Ok(resolved) => resolved,
            Err(_) => return analyze_import(context, request),
        };
        let key = cache_key_for_resolved_import(request, &resolved);

        if let Some(result) = self.cache.get(&key) {
            return result;
        }

        let result = analyze_resolved_import(context, request, resolved.clone());

        if should_cache_result(&result) {
            let fingerprints = dependency_fingerprints(request, &resolved, &result);
            self.cache_full_variant_alias(request, &result, &resolved, &fingerprints);
            self.cache
                .insert_with_fingerprints(key, result.clone(), fingerprints);
        }

        result
    }

    fn cache_full_variant_alias(
        &self,
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

        if self.cache.get(&namespace_key).is_some() {
            return;
        }

        let mut namespace_result = result.clone();
        namespace_result.cache_hit = false;
        namespace_result.truly_treeshakeable = false;
        self.cache.insert_with_fingerprints(
            namespace_key,
            namespace_result,
            dependency_fingerprints.to_vec(),
        );
    }
}

fn should_cache_result(result: &ImportResult) -> bool {
    result.error.is_none() && !has_request_specific_diagnostics(result)
}

fn detected_imports_for_document(
    active_document_path: &str,
    source: &str,
    apply_ignore_rules: bool,
) -> Result<Vec<DetectedImport>, String> {
    let mut imports = analyze_imports(active_document_path, source)?;

    if apply_ignore_rules {
        let active_path = Path::new(active_document_path);
        let rules = load_import_lens_ignore(active_path);
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

fn protocol_diagnostics(stage: &str, message: &str) -> Vec<ImportDiagnostic> {
    vec![ImportDiagnostic {
        stage: stage.to_owned(),
        message: message.to_owned(),
        details: Vec::new(),
    }]
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

fn is_supported_protocol_version(version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&version)
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
