use crate::{
    ipc::protocol::{ImportKind, ImportRequest, ImportResult},
    pipeline::compress::compress_all,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct AnalysisContext {
    pub workspace_root: PathBuf,
    pub active_document_path: PathBuf,
}

#[derive(Debug, Clone)]
struct PackageManifest {
    root: PathBuf,
    json: Value,
}

pub fn analyze_import(context: &AnalysisContext, request: &ImportRequest) -> ImportResult {
    match analyze_import_inner(context, request) {
        Ok(result) => result,
        Err(error) => error_result(request, error),
    }
}

fn analyze_import_inner(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ImportResult, String> {
    let manifest = find_package_manifest(context, &request.package_name)?;
    let side_effects = side_effects(&manifest.json);
    let (entry_path, is_cjs) = resolve_entry_path(&manifest, request)?;
    let source = fs::read_to_string(&entry_path).map_err(|error| {
        format!(
            "failed to read package entry {}: {error}",
            entry_path.display()
        )
    })?;
    let minified = estimate_minified_source(&source);
    let compressed = compress_all(&minified)
        .map_err(|error| format!("failed to compress minified output: {error}"))?;
    let raw_bytes = source.len() as u64;
    let minified_bytes = minified.len() as u64;

    Ok(ImportResult {
        specifier: request.specifier.clone(),
        raw_bytes,
        minified_bytes,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        cache_hit: false,
        side_effects,
        truly_treeshakeable: !side_effects
            && !is_cjs
            && matches!(request.import_kind, ImportKind::Named),
        is_cjs,
        error: None,
    })
}

fn error_result(request: &ImportRequest, message: String) -> ImportResult {
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
        error: Some(message),
    }
}

fn find_package_manifest(
    context: &AnalysisContext,
    package_name: &str,
) -> Result<PackageManifest, String> {
    validate_package_name(package_name)?;

    let mut current = context
        .active_document_path
        .parent()
        .ok_or_else(|| "active document path has no parent directory".to_owned())?
        .to_path_buf();

    loop {
        let package_root = current.join("node_modules").join(package_name);
        let package_json_path = package_root.join("package.json");

        if package_json_path.exists() {
            let json = serde_json::from_str::<Value>(
                &fs::read_to_string(&package_json_path)
                    .map_err(|error| format!("failed to read package manifest: {error}"))?,
            )
            .map_err(|error| format!("failed to parse package manifest: {error}"))?;
            return Ok(PackageManifest {
                root: package_root,
                json,
            });
        }

        if !current.pop() {
            break;
        }
    }

    Err(format!("package manifest not found for {package_name}"))
}

fn validate_package_name(package_name: &str) -> Result<(), String> {
    let parts = package_name.split('/').collect::<Vec<_>>();
    let is_valid = if package_name.starts_with('@') {
        parts.len() == 2
            && parts[0].len() > 1
            && is_safe_package_segment(parts[0])
            && is_safe_package_segment(parts[1])
    } else {
        parts.len() == 1 && is_safe_package_segment(parts[0])
    };

    if is_valid {
        return Ok(());
    }

    Err(format!("unsafe package name: {package_name}"))
}

fn is_safe_package_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains('\\')
        && !segment.contains(':')
}

fn resolve_entry_path(
    manifest: &PackageManifest,
    request: &ImportRequest,
) -> Result<(PathBuf, bool), String> {
    if let Some(subpath) = subpath_for_request(request) {
        return resolve_file_candidate(&manifest.root.join(subpath)).map(|path| (path, false));
    }

    if let Some(module) = manifest.json.get("module").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(module)).map(|path| (path, false));
    }

    if let Some(browser) = manifest.json.get("browser").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(browser)).map(|path| (path, false));
    }

    if let Some(main) = manifest.json.get("main").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(main)).map(|path| (path, true));
    }

    resolve_file_candidate(&manifest.root.join("index.js")).map(|path| (path, true))
}

fn subpath_for_request(request: &ImportRequest) -> Option<&str> {
    request
        .specifier
        .strip_prefix(&request.package_name)
        .and_then(|value| value.strip_prefix('/'))
}

fn resolve_file_candidate(candidate: &Path) -> Result<PathBuf, String> {
    let candidates = [
        candidate.to_path_buf(),
        append_extension(candidate, "js"),
        append_extension(candidate, "mjs"),
        append_extension(candidate, "cjs"),
        candidate.join("index.js"),
        candidate.join("index.mjs"),
        candidate.join("index.cjs"),
    ];

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| format!("package entry not found near {}", candidate.display()))
}

fn append_extension(candidate: &Path, extension: &str) -> PathBuf {
    let mut path = candidate.as_os_str().to_owned();
    path.push(".");
    path.push(extension);
    PathBuf::from(path)
}

fn side_effects(package_json: &Value) -> bool {
    match package_json.get("sideEffects") {
        Some(Value::Bool(value)) => *value,
        Some(Value::Array(_)) => true,
        Some(_) => true,
        None => true,
    }
}

fn estimate_minified_source(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}
