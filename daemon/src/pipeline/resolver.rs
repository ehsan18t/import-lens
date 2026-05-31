use crate::ipc::protocol::ImportRequest;
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

pub fn resolve_package_entry(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<ResolvedPackage, String> {
    validate_package_name(&request.package_name)?;

    let manifest = find_package_manifest(active_document_path, request)?;
    let resolution = if manifest.json.get("exports").is_some() {
        resolve_with_oxc(active_document_path, request)
    } else {
        Err("package has no exports map; using legacy entry fallback".to_owned())
    };
    let (entry_path, is_cjs) = match resolution {
        Ok(resolved) => {
            let entry_path = resolved.entry_path;
            let is_cjs = (resolved.is_cjs || entry_looks_commonjs(&entry_path))
                && !entry_matches_manifest_esm_field(&manifest, &entry_path);
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

    let resolver = Resolver::new(ResolveOptions {
        alias_fields: vec![vec!["browser".to_owned()]],
        condition_names: vec![
            "module".to_owned(),
            "import".to_owned(),
            "default".to_owned(),
        ],
        extensions: vec![".js".to_owned(), ".mjs".to_owned(), ".cjs".to_owned()],
        main_fields: vec!["browser".to_owned(), "module".to_owned()],
        module_type: true,
        node_path: false,
        ..ResolveOptions::default()
    });

    let resolution = resolver
        .resolve(directory, &request.specifier)
        .map_err(|error| error.to_string())?;

    if resolution.query().is_some() || resolution.fragment().is_some() {
        return Err(format!(
            "resolved entry contains unsupported query or fragment: {}",
            resolution.full_path().display()
        ));
    }

    let is_cjs = resolution.module_type() == Some(ModuleType::CommonJs)
        || path_looks_cjs(&resolution.full_path().to_string_lossy());

    Ok(ResolvedEntry {
        entry_path: resolution.into_path_buf(),
        is_cjs,
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
            return resolve_file_candidate(&manifest.root.join(main)).map(|path| (path, true));
        }

        return Err(format!(
            "failed to resolve package entry with oxc_resolver: {resolution_error}"
        ));
    }

    if let Some(sub) = subpath {
        return resolve_file_candidate(&manifest.root.join(sub)).map(|path| (path, false));
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
        .filter_map(|relative| resolve_file_candidate(&manifest.root.join(relative)).ok())
        .any(|candidate| candidate == entry_path)
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

    let path_str = found_path.to_string_lossy();
    if path_str.ends_with(".ts") || path_str.ends_with(".tsx") {
        return Err(format!(
            "resolved entry is a TypeScript source file ({}) which cannot be analyzed",
            found_path.display()
        ));
    }

    Ok(found_path)
}

pub(crate) fn append_extension(candidate: &Path, extension: &str) -> PathBuf {
    let mut path = candidate.as_os_str().to_owned();
    path.push(".");
    path.push(extension);
    PathBuf::from(path)
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

fn entry_looks_commonjs(entry_path: &Path) -> bool {
    let Ok(source) = fs::read_to_string(entry_path) else {
        return false;
    };

    source.contains("module.exports")
        || source.contains("exports.")
        || source.contains("require(")
        || source.contains("require (")
}
