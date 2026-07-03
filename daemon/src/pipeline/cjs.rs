use crate::{
    ipc::protocol::{ImportDiagnostic, ImportRuntime, ModuleContribution},
    pipeline::{
        cjs_scan::scan_cjs_source,
        graph::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES},
        resolver::{normalize_existing_path, resolve_module_path, shared_resolvers},
    },
};
use oxc_resolver::Resolver;
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct CjsGraphAnalysis {
    pub source: String,
    pub module_breakdown: Vec<ModuleContribution>,
    pub full_module_breakdown: Vec<ModuleContribution>,
    pub exports: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub unsupported: bool,
}

pub fn analyze_cjs_graph_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<CjsGraphAnalysis, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    let mut queue = VecDeque::from([entry_path]);
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    let mut module_breakdown = Vec::new();
    let mut exports = Vec::new();
    let mut diagnostics = Vec::new();
    let mut total_source_bytes = 0_usize;
    let mut unsupported = false;
    let resolvers = shared_resolvers();
    let resolver = resolvers.resolver(runtime);

    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        if seen.len() > MAX_GRAPH_MODULES {
            return Err(format!(
                "CommonJS module count limit exceeded while loading {}; limit: {}",
                path.display(),
                MAX_GRAPH_MODULES
            ));
        }

        let source = fs::read_to_string(&path).map_err(|error| {
            format!("failed to read CommonJS module {}: {error}", path.display())
        })?;
        let source_bytes = source.len();
        if source_bytes > MAX_MODULE_SOURCE_BYTES {
            return Err(format!(
                "CommonJS module source size {} exceeds limit {} in {}",
                source_bytes,
                MAX_MODULE_SOURCE_BYTES,
                path.display()
            ));
        }
        total_source_bytes = total_source_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| format!("CommonJS graph source size overflow in {}", path.display()))?;
        if total_source_bytes > MAX_GRAPH_SOURCE_BYTES {
            return Err(format!(
                "CommonJS graph source size {} exceeds limit {} while loading {}",
                total_source_bytes,
                MAX_GRAPH_SOURCE_BYTES,
                path.display()
            ));
        }

        let scan = scan_cjs_source(&source);
        unsupported |= scan.unsupported;
        for specifier in scan.requires {
            match resolve_require(resolver, &path, &specifier) {
                Ok(Some(resolved_path)) => queue.push_back(resolved_path),
                Ok(None) => diagnostics.push(diagnostic(
                    "cjs_resolution",
                    format!("CommonJS require '{specifier}' was kept external"),
                    vec![format!("from_path: {}", path.display())],
                )),
                Err(error) => {
                    diagnostics.push(diagnostic(
                        "cjs_resolution",
                        error,
                        vec![format!("from_path: {}", path.display())],
                    ));
                    unsupported = true;
                }
            }
        }

        if seen.len() == 1 {
            exports.extend(scan.exports);
        }

        module_breakdown.push(ModuleContribution {
            path: path.to_string_lossy().to_string(),
            bytes: source_bytes as u64,
        });
        sources.push(format!(";(() => {{\n{source}\n}})();"));
    }

    exports.sort();
    exports.dedup();
    module_breakdown.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    let full_module_breakdown = module_breakdown.clone();
    module_breakdown.truncate(10);

    Ok(CjsGraphAnalysis {
        source: sources.join("\n"),
        module_breakdown,
        full_module_breakdown,
        exports,
        diagnostics,
        unsupported,
    })
}

fn resolve_require(
    resolver: &Resolver,
    from_path: &Path,
    specifier: &str,
) -> Result<Option<PathBuf>, String> {
    if !specifier.starts_with('.') {
        return Ok(None);
    }

    let from_dir = from_path.parent().ok_or_else(|| {
        format!(
            "CommonJS module path has no parent directory: {}",
            from_path.display()
        )
    })?;
    resolve_module_path(resolver, from_dir, specifier)
        .map(|resolved| Some(resolved.path))
        .map_err(|error| {
            format!(
                "failed to resolve CommonJS require '{specifier}' from {}: {error}",
                from_path.display()
            )
        })
}

fn diagnostic(stage: &str, message: String, details: Vec<String>) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: stage.to_owned(),
        message,
        details,
    }
}
