use crate::{
    cache::{key::cache_key_for_import, memory::ImportCache},
    ipc::protocol::{
        BatchRequest, BatchResponse, ImportDiagnostic, ImportKind, ImportRequest, ImportResult,
        PROTOCOL_VERSION,
    },
    pipeline::analyze::{AnalysisContext, analyze_import, analyze_resolved_import},
    pipeline::graph::{clear_module_graph_cache, invalidate_module_graph_cache_for_package},
    pipeline::resolver::ResolvedPackage,
};
use rayon::prelude::*;
use std::path::PathBuf;

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
        let imports = request
            .imports
            .par_iter()
            .map(|item| self.analyze_with_cache(&context, item))
            .collect();

        BatchResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            indexes: None,
        }
    }

    pub fn handle_batch_streaming(&self, request: BatchRequest) -> Vec<BatchResponse> {
        if !is_supported_protocol_version(request.version) {
            return vec![protocol_error_batch_response(
                &request,
                format!("unsupported protocol version {}", request.version),
            )];
        }

        if request.version < 2 || !request.streaming {
            return vec![self.handle_batch(request)];
        }

        let context = AnalysisContext {
            workspace_root: PathBuf::from(&request.workspace_root),
            active_document_path: PathBuf::from(&request.active_document_path),
        };
        let mut imports = Vec::with_capacity(request.imports.len());
        let mut responses = Vec::with_capacity(request.imports.len() + 1);

        for (index, item) in request.imports.iter().enumerate() {
            let result = self.analyze_with_cache(&context, item);
            imports.push(result.clone());
            responses.push(BatchResponse {
                version: request.version,
                request_id: request.request_id,
                imports: vec![result],
                indexes: Some(vec![index]),
            });
        }

        responses.push(BatchResponse {
            version: request.version,
            request_id: request.request_id,
            imports,
            indexes: None,
        });
        responses
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

    pub fn prewarm_import<F>(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
        should_continue: F,
    ) where
        F: Fn() -> bool,
    {
        let key = cache_key_for_import(request);

        if self.cache.get(&key).is_some() || !should_continue() {
            return;
        }

        let result = analyze_import(context, request);

        if result.error.is_none() && should_continue() {
            self.cache_full_variant_alias(request, &result);
            self.cache.insert(key, result);
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
        let key = cache_key_for_import(request);

        if self.cache.get(&key).is_some() || !should_continue() {
            return;
        }

        let result = analyze_resolved_import(context, request, resolved);

        if result.error.is_none() && should_continue() {
            self.cache_full_variant_alias(request, &result);
            self.cache.insert(key, result);
        }
    }

    fn analyze_with_cache(
        &self,
        context: &AnalysisContext,
        request: &ImportRequest,
    ) -> ImportResult {
        let key = cache_key_for_import(request);

        if let Some(result) = self.cache.get(&key) {
            return result;
        }

        let result = analyze_import(context, request);

        if result.error.is_none() {
            self.cache_full_variant_alias(request, &result);
            self.cache.insert(key, result.clone());
        }

        result
    }

    fn cache_full_variant_alias(&self, request: &ImportRequest, result: &ImportResult) {
        if !result.side_effects
            || result.is_cjs
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
        let namespace_key = cache_key_for_import(&namespace_request);

        if self.cache.get(&namespace_key).is_some() {
            return;
        }

        let mut namespace_result = result.clone();
        namespace_result.cache_hit = false;
        namespace_result.truly_treeshakeable = false;
        self.cache.insert(namespace_key, namespace_result);
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

fn is_supported_protocol_version(version: u32) -> bool {
    (1..=PROTOCOL_VERSION).contains(&version)
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
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: "protocol".to_owned(),
            message,
            details: vec![format!("specifier: {}", request.specifier)],
        }],
    }
}
