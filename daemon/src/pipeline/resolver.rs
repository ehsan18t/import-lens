use crate::ipc::protocol::{ImportRequest, ImportRuntime};
use oxc_resolver::{ModuleType, ResolveOptions, Resolver};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub package_root: PathBuf,
    pub package_json: Value,
    pub entry_path: PathBuf,
    pub is_cjs: bool,
    pub side_effects: SideEffectsMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffectsMode {
    False,
    True,
    Array,
    Missing,
    Unknown,
}

impl SideEffectsMode {
    pub fn has_side_effects(self) -> bool {
        !matches!(self, Self::False)
    }
}

#[derive(Debug, Clone)]
struct PackageManifest {
    root: PathBuf,
    json: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedModulePath {
    pub path: PathBuf,
    pub is_cjs: bool,
}

pub fn resolve_package_entry(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<ResolvedPackage, String> {
    validate_package_name(&request.package_name)?;

    let manifest = find_package_manifest(active_document_path, request)?;
    let resolution = resolve_with_oxc(active_document_path, request);
    let (entry_path, is_cjs) = match resolution {
        Ok(resolved) => {
            let entry_path = resolved.entry_path;
            validate_declared_entry_resolution(&manifest, request.runtime)?;
            let is_cjs = resolved_entry_is_commonjs(&manifest, &entry_path, resolved.is_cjs);
            (entry_path, is_cjs)
        }
        Err(error) => resolve_legacy_fallback(&manifest, request, &error)?,
    };

    Ok(ResolvedPackage {
        package_root: manifest.root,
        side_effects: side_effects_mode(&manifest.json),
        package_json: manifest.json,
        entry_path,
        is_cjs,
    })
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    entry_path: PathBuf,
    is_cjs: bool,
}

fn resolve_with_oxc(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<ResolvedEntry, String> {
    let directory = active_document_path
        .parent()
        .ok_or_else(|| "active document path has no parent directory".to_owned())?;

    let resolver = create_resolver(request.runtime);
    let resolved = resolve_module_path(&resolver, directory, &request.specifier)?;

    Ok(ResolvedEntry {
        entry_path: resolved.path,
        is_cjs: resolved.is_cjs,
    })
}

fn resolve_legacy_fallback(
    manifest: &PackageManifest,
    request: &ImportRequest,
    resolution_error: &str,
) -> Result<(PathBuf, bool), String> {
    let subpath = subpath_for_request(request);
    if manifest.json.get("exports").is_some() {
        if let Some(sub) = subpath {
            let exports_key = format!("./{sub}");
            return Err(format!(
                "subpath '{}' is not defined in the exports map of {}",
                exports_key, request.package_name
            ));
        }

        if let Some(main) = manifest.json.get("main").and_then(Value::as_str) {
            return resolve_file_candidate(&manifest.root.join(main))
                .map(|path| classify_resolved_entry(manifest, path, false));
        }

        return Err(format!(
            "failed to resolve package entry with oxc_resolver: {resolution_error}"
        ));
    }

    if let Some(sub) = subpath {
        return resolve_file_candidate(&manifest.root.join(sub))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    if let Some(module) = manifest.json.get("module").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(module))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    if let Some(browser) = manifest.json.get("browser").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(browser))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    if let Some(main) = manifest.json.get("main").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(main))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    resolve_file_candidate(&manifest.root.join("index.js"))
        .map(|path| classify_resolved_entry(manifest, path, false))
}

fn validate_declared_entry_resolution(
    manifest: &PackageManifest,
    runtime: ImportRuntime,
) -> Result<(), String> {
    if manifest.json.get("exports").is_some() {
        return Ok(());
    }

    let declared_entries = profile_entry_fields(runtime)
        .iter()
        .filter_map(|field| {
            manifest
                .json
                .get(*field)
                .and_then(Value::as_str)
                .map(|target| (*field, target))
        })
        .collect::<Vec<_>>();
    if declared_entries.is_empty() {
        return Ok(());
    }

    let resolver = create_resolver(runtime);
    for (_, target) in &declared_entries {
        if resolve_manifest_target(&resolver, &manifest.root, target).is_ok() {
            return Ok(());
        }
    }

    let (_, first_target) = declared_entries[0];
    resolve_file_candidate(&manifest.root.join(first_target)).map(|_| ())
}

fn profile_entry_fields(runtime: ImportRuntime) -> &'static [&'static str] {
    match runtime {
        ImportRuntime::Component | ImportRuntime::Client => &["browser", "module", "main"],
        ImportRuntime::Server => &["module", "main"],
    }
}

fn resolve_manifest_target(
    resolver: &Resolver,
    package_root: &Path,
    target: &str,
) -> Result<ResolvedModulePath, String> {
    let specifier =
        if target.starts_with("./") || target.starts_with("../") || Path::new(target).is_absolute()
        {
            target.to_owned()
        } else {
            format!("./{target}")
        };

    resolve_module_path(resolver, package_root, &specifier)
}

fn find_package_manifest(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<PackageManifest, String> {
    let mut current = active_document_path
        .parent()
        .ok_or_else(|| "active document path has no parent directory".to_owned())?
        .to_path_buf();
    let mut checked_paths = Vec::new();

    loop {
        let package_root = current.join("node_modules").join(&request.package_name);
        let package_json_path = package_root.join("package.json");
        checked_paths.push(format!("checked: {}", package_json_path.display()));

        if package_json_path.exists() {
            let json = serde_json::from_str::<Value>(
                &fs::read_to_string(&package_json_path).map_err(|error| {
                    format!(
                        "failed to read package manifest {}: {error}",
                        package_json_path.display()
                    )
                })?,
            )
            .map_err(|error| {
                format!(
                    "failed to parse package manifest {}: {error}",
                    package_json_path.display()
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

    Err(format!(
        "package manifest not found for {}; {}",
        request.package_name,
        checked_paths.join("; ")
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

fn entry_matches_manifest_esm_field(manifest: &PackageManifest, entry_path: &Path) -> bool {
    ["module", "browser"]
        .iter()
        .filter_map(|field| manifest.json.get(field).and_then(Value::as_str))
        .filter_map(|relative| resolve_manifest_file_candidate(manifest, relative).ok())
        .any(|candidate| candidate == entry_path)
}

fn classify_resolved_entry(
    manifest: &PackageManifest,
    entry_path: PathBuf,
    resolver_is_cjs: bool,
) -> (PathBuf, bool) {
    let is_cjs = resolved_entry_is_commonjs(manifest, &entry_path, resolver_is_cjs);
    (entry_path, is_cjs)
}

fn resolved_entry_is_commonjs(
    manifest: &PackageManifest,
    entry_path: &Path,
    resolver_is_cjs: bool,
) -> bool {
    let Some(extension) = path_extension(entry_path) else {
        return resolver_is_cjs;
    };

    if extension == "cjs" {
        return true;
    }
    if extension == "mjs" {
        return false;
    }
    if matches!(extension, "ts" | "tsx" | "jsx" | "json") {
        return false;
    }
    if entry_matches_manifest_esm_field(manifest, entry_path) {
        return false;
    }
    if entry_matches_exports_condition(manifest, entry_path, &["browser", "import"]) {
        return false;
    }
    if entry_matches_exports_condition(manifest, entry_path, &["require"]) {
        return true;
    }

    match package_type(&manifest.json) {
        Some("module") => return false,
        Some("commonjs") => return true,
        _ => {}
    }

    resolver_is_cjs || entry_matches_manifest_main_field(manifest, entry_path) || extension == "js"
}

fn path_extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|extension| extension.to_str())
}

fn entry_matches_manifest_main_field(manifest: &PackageManifest, entry_path: &Path) -> bool {
    manifest
        .json
        .get("main")
        .and_then(Value::as_str)
        .and_then(|relative| resolve_manifest_file_candidate(manifest, relative).ok())
        .is_some_and(|candidate| candidate == entry_path)
}

fn package_type(package_json: &Value) -> Option<&str> {
    package_json.get("type").and_then(Value::as_str)
}

fn entry_matches_exports_condition(
    manifest: &PackageManifest,
    entry_path: &Path,
    conditions: &[&str],
) -> bool {
    manifest.json.get("exports").is_some_and(|exports| {
        exports_condition_points_to_entry(exports, manifest, entry_path, conditions)
    })
}

fn exports_condition_points_to_entry(
    value: &Value,
    manifest: &PackageManifest,
    entry_path: &Path,
    conditions: &[&str],
) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };

    map.iter().any(|(key, child)| {
        if conditions.contains(&key.as_str())
            && exports_target_points_to_entry(child, manifest, entry_path)
        {
            return true;
        }

        exports_condition_points_to_entry(child, manifest, entry_path, conditions)
    })
}

fn exports_target_points_to_entry(
    value: &Value,
    manifest: &PackageManifest,
    entry_path: &Path,
) -> bool {
    match value {
        Value::String(target) => resolve_manifest_file_candidate(manifest, target)
            .is_ok_and(|candidate| candidate == entry_path),
        Value::Array(items) => items
            .iter()
            .any(|item| exports_target_points_to_entry(item, manifest, entry_path)),
        Value::Object(map) => map
            .values()
            .any(|item| exports_target_points_to_entry(item, manifest, entry_path)),
        _ => false,
    }
}

fn resolve_manifest_file_candidate(
    manifest: &PackageManifest,
    relative: &str,
) -> Result<PathBuf, String> {
    let candidate = resolve_file_candidate(&manifest.root.join(relative))?;
    normalize_existing_path(&candidate)
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

    let found_path = candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            let details = candidates
                .iter()
                .map(|path| format!("candidate: {}", path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "package entry not found near {}; {details}",
                candidate.display()
            )
        })?;

    Ok(found_path)
}

pub(crate) fn create_resolver(runtime: ImportRuntime) -> Resolver {
    Resolver::new(resolve_options(runtime))
}

fn resolve_options(runtime: ImportRuntime) -> ResolveOptions {
    match runtime {
        ImportRuntime::Component | ImportRuntime::Client => ResolveOptions {
            alias_fields: vec![vec!["browser".to_owned()]],
            condition_names: vec![
                "browser".to_owned(),
                "module".to_owned(),
                "import".to_owned(),
                "default".to_owned(),
            ],
            extensions: module_extensions(),
            main_fields: vec!["browser".to_owned(), "module".to_owned(), "main".to_owned()],
            module_type: true,
            node_path: false,
            ..ResolveOptions::default()
        },
        ImportRuntime::Server => ResolveOptions {
            alias_fields: Vec::new(),
            condition_names: vec![
                "node".to_owned(),
                "server".to_owned(),
                "module".to_owned(),
                "import".to_owned(),
                "default".to_owned(),
            ],
            extensions: module_extensions(),
            main_fields: vec!["module".to_owned(), "main".to_owned()],
            module_type: true,
            node_path: false,
            ..ResolveOptions::default()
        },
    }
}

fn module_extensions() -> Vec<String> {
    [".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx", ".json"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

pub(crate) fn resolve_module_path(
    resolver: &Resolver,
    from_path: &Path,
    specifier: &str,
) -> Result<ResolvedModulePath, String> {
    let resolution = resolver
        .resolve(from_path, specifier)
        .map_err(|error| error.to_string())?;

    if resolution.query().is_some() || resolution.fragment().is_some() {
        return Err(format!(
            "resolved module '{specifier}' contains unsupported query or fragment"
        ));
    }

    let full_path = resolution.full_path().to_path_buf();
    if !full_path.is_file() {
        return Err(format!(
            "resolved module '{specifier}' is not a file: {}",
            full_path.display()
        ));
    }

    let is_cjs = resolution.module_type() == Some(ModuleType::CommonJs)
        || path_looks_cjs(&full_path.to_string_lossy());
    Ok(ResolvedModulePath {
        path: normalize_existing_path(&full_path)?,
        is_cjs,
    })
}

pub(crate) fn append_extension(candidate: &Path, extension: &str) -> PathBuf {
    let mut path = candidate.as_os_str().to_owned();
    path.push(".");
    path.push(extension);
    PathBuf::from(path)
}

pub(crate) fn normalize_existing_path(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve path {}: {error}", path.display()))
}

fn side_effects_mode(package_json: &Value) -> SideEffectsMode {
    match package_json.get("sideEffects") {
        Some(Value::Bool(false)) => SideEffectsMode::False,
        Some(Value::Bool(true)) => SideEffectsMode::True,
        Some(Value::Array(_)) => SideEffectsMode::Array,
        Some(_) => SideEffectsMode::Unknown,
        None => SideEffectsMode::Missing,
    }
}

fn subpath_for_request(request: &ImportRequest) -> Option<&str> {
    request
        .specifier
        .strip_prefix(&request.package_name)
        .and_then(|value| value.strip_prefix('/'))
}

fn path_looks_cjs(path: &str) -> bool {
    path.ends_with(".cjs")
}
