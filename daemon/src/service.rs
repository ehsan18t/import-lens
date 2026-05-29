use crate::{
    cache::{key::cache_key_for_import, memory::ImportCache},
    ipc::protocol::{
        BatchRequest, BatchResponse, ImportDiagnostic, ImportRequest, ImportResult,
        PROTOCOL_VERSION,
    },
    pipeline::analyze::{AnalysisContext, analyze_import},
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
        if request.version != PROTOCOL_VERSION {
            return BatchResponse {
                version: PROTOCOL_VERSION,
                request_id: request.request_id,
                imports: request
                    .imports
                    .iter()
                    .map(|item| {
                        protocol_error(
                            item,
                            format!("unsupported protocol version {}", request.version),
                        )
                    })
                    .collect(),
            };
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
            version: PROTOCOL_VERSION,
            request_id: request.request_id,
            imports,
        }
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.cache.invalidate_package(package_name);
    }

    pub fn invalidate_all(&self) {
        self.cache.clear();
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
            self.cache.insert(key, result.clone());
        }

        result
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
        error: Some(message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: "protocol".to_owned(),
            message,
            details: vec![format!("specifier: {}", request.specifier)],
        }],
    }
}
