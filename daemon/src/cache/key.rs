use crate::{
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::resolver::ResolvedPackage,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub const CACHE_KEY_PREFIX_V3: &str = "v3:";
pub const ANALYZER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub len: u64,
    pub modified_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheIdentityV3 {
    pub analyzer_version: String,
    pub specifier: String,
    pub package_name: String,
    pub package_version: String,
    pub package_root: Option<String>,
    pub entry_path: Option<String>,
    pub runtime: ImportRuntime,
    pub import_kind: ImportKind,
    pub named_exports: Vec<String>,
    pub manifest_fingerprint: Option<FileFingerprint>,
    pub entry_fingerprint: Option<FileFingerprint>,
}

pub fn cache_key_for_resolved_import(
    request: &ImportRequest,
    resolved: &ResolvedPackage,
) -> String {
    encode_cache_identity(&cache_identity_for_import(request, Some(resolved)))
}

fn cache_identity_for_import(
    request: &ImportRequest,
    resolved: Option<&ResolvedPackage>,
) -> CacheIdentityV3 {
    let mut named_exports = if matches!(&request.import_kind, ImportKind::Named) {
        request.named.clone()
    } else {
        Vec::new()
    };
    named_exports.sort();
    named_exports.dedup();

    CacheIdentityV3 {
        analyzer_version: ANALYZER_VERSION.to_owned(),
        specifier: request.specifier.clone(),
        package_name: request.package_name.clone(),
        package_version: request.version.clone(),
        package_root: resolved.map(|package| normalize_identity_path(&package.package_root)),
        entry_path: resolved.map(|package| normalize_identity_path(&package.entry_path)),
        runtime: request.runtime,
        import_kind: request.import_kind.clone(),
        named_exports,
        manifest_fingerprint: resolved
            .and_then(|package| file_fingerprint(package.package_root.join("package.json"))),
        entry_fingerprint: resolved.and_then(|package| file_fingerprint(&package.entry_path)),
    }
}

pub fn decode_cache_identity(key: &str) -> Option<CacheIdentityV3> {
    let encoded = key.strip_prefix(CACHE_KEY_PREFIX_V3)?;
    let bytes = hex_decode(encoded)?;
    rmp_serde::from_slice(&bytes).ok()
}

pub fn cache_key_matches_package(key: &str, package_name: &str) -> bool {
    if let Some(identity) = decode_cache_identity(key) {
        return identity.package_name == package_name;
    }

    let root_prefix = format!("{package_name}@");
    let subpath_prefix = format!("{package_name}/");
    key.starts_with(&root_prefix) || key.starts_with(&subpath_prefix)
}

pub fn fingerprints_for_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<FileFingerprint> {
    let mut fingerprints = paths
        .into_iter()
        .filter_map(file_fingerprint)
        .collect::<Vec<_>>();
    fingerprints.sort_by(|left, right| left.path.cmp(&right.path));
    fingerprints.dedup_by(|left, right| left.path == right.path);
    fingerprints
}

pub fn fingerprints_are_current(fingerprints: &[FileFingerprint]) -> bool {
    fingerprints.iter().all(|stored| {
        file_fingerprint(&stored.path).is_some_and(|current| {
            current.len == stored.len && current.modified_millis == stored.modified_millis
        })
    })
}

fn encode_cache_identity(identity: &CacheIdentityV3) -> String {
    let bytes = rmp_serde::to_vec(identity).unwrap_or_default();
    format!("{CACHE_KEY_PREFIX_V3}{}", hex_encode(&bytes))
}

fn file_fingerprint(path: impl AsRef<Path>) -> Option<FileFingerprint> {
    let path = path.as_ref();
    let metadata = fs::metadata(path).ok()?;
    let modified_millis = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default();

    Some(FileFingerprint {
        path: normalize_identity_path(path),
        len: metadata.len(),
        modified_millis,
    })
}

fn normalize_identity_path(path: impl AsRef<Path>) -> String {
    fs::canonicalize(path.as_ref())
        .unwrap_or_else(|_| PathBuf::from(path.as_ref()))
        .to_string_lossy()
        .replace('\\', "/")
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }

    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let high = hex_value(chunk[0])?;
            let low = hex_value(chunk[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
