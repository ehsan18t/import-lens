use crate::{
    cache::{
        key::{cache_key_for_resolved_import, fingerprints_for_paths},
        memory::ImportCache,
    },
    ipc::protocol::{
        BatchRequest, BatchResponse, ConfidenceLevel, EnumerateExportsRequest,
        EnumerateExportsResponse, FileSizeRequest, FileSizeResponse, ImportDiagnostic, ImportKind,
        ImportRequest, ImportResult, ImportRuntime, PROTOCOL_VERSION,
    },
    pipeline::analyze::{AnalysisContext, analyze_import, analyze_resolved_import},
    pipeline::file_size::{annotate_shared_bytes, compute_file_size},
    pipeline::graph::{
        ModuleGraph, ModuleId, build_module_graph_cached, build_module_graph_cached_with_runtime,
        clear_module_graph_cache, invalidate_module_graph_cache_for_package,
    },
    pipeline::resolver::{ResolvedPackage, resolve_package_entry},
};
use rayon::prelude::*;
use std::{collections::HashSet, path::PathBuf};

#[derive(Debug, Default)]
pub struct ImportLensService {
    cache: ImportCache,
}

impl ImportLensService {
    pub fn new(storage_path: Option<PathBuf>, enable_disk_cache: bool) -> Self {
        Self {
            cache: ImportCache::new(storage_path, enable_disk_cache),
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
