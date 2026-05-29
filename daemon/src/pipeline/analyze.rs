use crate::{
    ipc::protocol::{ImportDiagnostic, ImportKind, ImportRequest, ImportResult},
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

#[derive(Debug, Clone)]
struct AnalysisError {
    stage: &'static str,
    message: String,
    details: Vec<String>,
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
) -> Result<ImportResult, AnalysisError> {
    let manifest = find_package_manifest(context, request)?;
    let side_effects = side_effects(&manifest.json);
    let (entry_path, is_cjs) = resolve_entry_path(&manifest, context, request)?;
    let source = fs::read_to_string(&entry_path).map_err(|error| {
        error_with_context(
            "entry_read",
            format!(
                "failed to read package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;
    let minified = estimate_minified_source(&source);
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            "compression",
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;
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
        diagnostics: Vec::new(),
    })
}

fn error_result(request: &ImportRequest, error: AnalysisError) -> ImportResult {
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
        error: Some(error.message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: error.stage.to_owned(),
            message: error.message,
            details: error.details,
        }],
    }
}

fn find_package_manifest(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<PackageManifest, AnalysisError> {
    validate_package_name(&request.package_name).map_err(|message| {
        error_with_context("package_validation", message, context, request, Vec::new())
    })?;

    let mut current = context
        .active_document_path
        .parent()
        .ok_or_else(|| {
            error_with_context(
                "package_resolution",
                "active document path has no parent directory",
                context,
                request,
                Vec::new(),
            )
        })?
        .to_path_buf();
    let mut checked_paths = Vec::new();

    loop {
        let package_root = current.join("node_modules").join(&request.package_name);
        let package_json_path = package_root.join("package.json");
        checked_paths.push(format!("checked: {}", package_json_path.display()));

        if package_json_path.exists() {
            let json = serde_json::from_str::<Value>(
                &fs::read_to_string(&package_json_path).map_err(|error| {
                    error_with_context(
                        "package_manifest",
                        format!("failed to read package manifest: {error}"),
                        context,
                        request,
                        vec![format!("manifest_path: {}", package_json_path.display())],
                    )
                })?,
            )
            .map_err(|error| {
                error_with_context(
                    "package_manifest",
                    format!("failed to parse package manifest: {error}"),
                    context,
                    request,
                    vec![format!("manifest_path: {}", package_json_path.display())],
                )
            })?;
            return Ok(PackageManifest {
                root: package_root,
                json,
            });
        }

        if !current.pop() {
            break;
        }
    }

    Err(error_with_context(
        "package_resolution",
        format!("package manifest not found for {}", request.package_name),
        context,
        request,
        checked_paths,
    ))
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
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<(PathBuf, bool), AnalysisError> {
    let subpath = subpath_for_request(request);
    let exports_key = match subpath {
        Some(sub) => format!("./{sub}"),
        None => ".".to_owned(),
    };

    // When the exports field is present it is the authoritative source for entry
    // resolution per the Node.js package specification.  Legacy fields (module,
    // browser, main) are only consulted when exports is absent.
    if let Some(exports) = manifest.json.get("exports") {
        if let Some((relative, is_cjs)) = resolve_exports_entry(exports, &exports_key) {
            return resolve_file_candidate(&manifest.root.join(&relative), context, request)
                .map(|path| (path, is_cjs));
        }

        // Wildcard pattern fallback: try "./*" when the exact key is not found.
        if subpath.is_some() {
            if let Some(pattern_target) = resolve_exports_wildcard(exports, subpath.unwrap()) {
                let is_cjs = path_looks_cjs(&pattern_target);
                return resolve_file_candidate(
                    &manifest.root.join(&pattern_target),
                    context,
                    request,
                )
                .map(|path| (path, is_cjs));
            }
        }

        // exports is present but neither exact key nor wildcard matched.
        return Err(error_with_context(
            "entry_resolution",
            format!(
                "subpath '{}' is not defined in the exports map of {}",
                exports_key, request.package_name
            ),
            context,
            request,
            vec![format!("exports_key: {exports_key}")],
        ));
    }

    // --- Legacy resolution (no exports field) ---

    if let Some(sub) = subpath {
        return resolve_file_candidate(&manifest.root.join(sub), context, request)
            .map(|path| (path, false));
    }

    if let Some(module) = manifest.json.get("module").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(module), context, request)
            .map(|path| (path, false));
    }

    if let Some(browser) = manifest.json.get("browser").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(browser), context, request)
            .map(|path| (path, false));
    }

    if let Some(main) = manifest.json.get("main").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(main), context, request)
            .map(|path| (path, true));
    }

    resolve_file_candidate(&manifest.root.join("index.js"), context, request)
        .map(|path| (path, true))
}

/// Condition keys tried in priority order.  `require` is intentionally absent
/// so we prefer ESM paths which are required for accurate tree-shaking.
const CONDITION_PRIORITY: &[&str] = &["module", "import", "browser", "default"];

/// Resolve an exact key from the `exports` field of package.json.
///
/// Returns `Some((relative_path, is_cjs))` on success.
fn resolve_exports_entry(exports: &Value, key: &str) -> Option<(String, bool)> {
    // String shorthand: `"exports": "./dist/index.mjs"`
    // This implicitly maps the root entry `"."`.
    if key == "." {
        if let Some(target) = exports.as_str() {
            return Some((target.to_owned(), path_looks_cjs(target)));
        }
    }

    // Object map: `"exports": { ".": ..., "./transition": ... }`
    if let Some(map) = exports.as_object() {
        // Determine whether the top-level keys are subpath keys (start with ".")
        // or condition keys (like "import", "default").  When a package has only
        // the root entry it may use conditions directly at the top level:
        //   "exports": { "import": "./index.mjs", "require": "./index.cjs" }
        let is_condition_map = map.keys().next().is_some_and(|k| !k.starts_with('.'));

        if is_condition_map && key == "." {
            return resolve_condition_value(exports);
        }

        if let Some(target) = map.get(key) {
            return resolve_export_target(target);
        }
    }

    // Array at top level: `"exports": [{ "import": "..." }, "./fallback.js"]`
    if key == "." {
        if let Some(arr) = exports.as_array() {
            return resolve_array_fallback(arr);
        }
    }

    None
}

/// Resolve a single export target which may be a string, a condition object, or
/// an array of fallbacks.
fn resolve_export_target(target: &Value) -> Option<(String, bool)> {
    if let Some(s) = target.as_str() {
        return Some((s.to_owned(), path_looks_cjs(s)));
    }

    if target.is_object() {
        return resolve_condition_value(target);
    }

    if let Some(arr) = target.as_array() {
        return resolve_array_fallback(arr);
    }

    None
}

/// Walk a condition object (`{ "import": "...", "default": "..." }`) using the
/// priority list and return the first matching target.
fn resolve_condition_value(value: &Value) -> Option<(String, bool)> {
    let map = value.as_object()?;

    for &condition in CONDITION_PRIORITY {
        if let Some(target) = map.get(condition) {
            if let Some(s) = target.as_str() {
                let is_cjs = condition == "require" || path_looks_cjs(s);
                return Some((s.to_owned(), is_cjs));
            }
            // Nested condition or array inside a condition key.
            if let Some(result) = resolve_export_target(target) {
                return Some(result);
            }
        }
    }

    None
}

/// Try each element of an array until one resolves.
fn resolve_array_fallback(arr: &[Value]) -> Option<(String, bool)> {
    for item in arr {
        if let Some(result) = resolve_export_target(item) {
            return Some(result);
        }
    }
    None
}

/// Resolve a subpath through wildcard patterns in the exports map.
///
/// Given `"./*"` → `"./dist/*.js"` and a subpath `"utils/foo"`, this produces
/// `"./dist/utils/foo.js"`.
fn resolve_exports_wildcard(exports: &Value, subpath: &str) -> Option<String> {
    let map = exports.as_object()?;

    for (pattern, target) in map {
        let Some(without_prefix) = pattern.strip_prefix("./") else {
            continue;
        };
        let Some(stem) = without_prefix.strip_suffix('*') else {
            continue;
        };
        if let Some(remainder) = subpath.strip_prefix(stem) {
            if let Some((resolved, _)) = resolve_export_target(target) {
                return Some(resolved.replace('*', remainder));
            }
        }
    }

    None
}

/// Heuristic: does this relative path look like a CommonJS file?
fn path_looks_cjs(path: &str) -> bool {
    path.ends_with(".cjs")
}

fn subpath_for_request(request: &ImportRequest) -> Option<&str> {
    request
        .specifier
        .strip_prefix(&request.package_name)
        .and_then(|value| value.strip_prefix('/'))
}

fn resolve_file_candidate(
    candidate: &Path,
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<PathBuf, AnalysisError> {
    let candidates = vec![
        candidate.to_path_buf(),
        append_extension(candidate, "js"),
        append_extension(candidate, "mjs"),
        append_extension(candidate, "cjs"),
        candidate.join("index.js"),
        candidate.join("index.mjs"),
        candidate.join("index.cjs"),
    ];

    let found_path = candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            error_with_context(
                "entry_resolution",
                format!("package entry not found near {}", candidate.display()),
                context,
                request,
                candidates
                    .iter()
                    .map(|path| format!("candidate: {}", path.display()))
                    .collect(),
            )
        })?;

    let path_str = found_path.to_string_lossy();
    if path_str.ends_with(".ts") || path_str.ends_with(".tsx") {
        return Err(error_with_context(
            "entry_resolution",
            format!(
                "resolved entry is a TypeScript source file ({}) which cannot be analyzed",
                found_path.display()
            ),
            context,
            request,
            vec![format!("resolved_entry: {}", found_path.display())],
        ));
    }

    Ok(found_path)
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

fn error_with_context(
    stage: &'static str,
    message: impl Into<String>,
    context: &AnalysisContext,
    request: &ImportRequest,
    details: Vec<String>,
) -> AnalysisError {
    let mut context_details = vec![
        format!("specifier: {}", request.specifier),
        format!("package: {}", request.package_name),
        format!(
            "active_document_path: {}",
            context.active_document_path.display()
        ),
        format!("workspace_root: {}", context.workspace_root.display()),
    ];
    context_details.extend(details);

    AnalysisError {
        stage,
        message: message.into(),
        details: context_details,
    }
}
