use crate::{
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::resolver::ResolvedPackage,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Fast, non-cryptographic content hash of the bytes read during analysis. Detects
/// real content changes and ignores no-op touches that only bump mtime. Not for
/// security.
pub fn content_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// The cache-key schema version. The key prefix (`v{N}:`) derives from it.
const CACHE_KEY_VERSION: u32 = 4;

/// The analyzer revision. A macro because `ANALYZER_VERSION` is built with `concat!`,
/// which only accepts literals; the macro keeps the literal spelled once.
///
/// **Bump this whenever a change can alter a reported size.** Every cached entry
/// records the revision it was computed under and is rejected when it differs.
///
/// Format: `<engine>-<minor line>.x+<revision>`. The minor line holds the patch as a
/// wildcard `x`. A Rolldown patch that does not move numbers needs no edit; one that
/// does, or any of our own number-moving changes, bumps `+<revision>`; a minor or major
/// bump changes the line itself (`rolldown-1.2.x+1`). Kept in step with
/// `daemon/Cargo.toml`'s pin by the `compiler-stack-upgrade` skill. Past values:
/// `git log -S analyzer_revision -- daemon/src/cache/key.rs`.
macro_rules! analyzer_revision {
    () => {
        "rolldown-1.2.x+28"
    };
}

pub const ANALYZER_REVISION: &str = analyzer_revision!();
pub const ANALYZER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", analyzer_revision!());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub len: u64,
    pub modified_millis: u64,
    /// xxh3 of the bytes read during analysis. Absent for a stat-only fingerprint
    /// (`file_fingerprint_with_hash(path, None)`). Not serialized when `None`, so a
    /// stat-only fingerprint encodes as a 3-element array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheIdentity {
    pub analyzer_version: String,
    pub specifier: String,
    pub package_name: String,
    pub package_version: String,
    pub package_root: Option<String>,
    pub entry_path: Option<String>,
    pub runtime: ImportRuntime,
    pub import_kind: ImportKind,
    pub named_exports: Vec<String>,
}

pub fn cache_key_for_resolved_import(
    request: &ImportRequest,
    resolved: &ResolvedPackage,
) -> String {
    encode_cache_identity(&cache_identity_for_import(request, Some(resolved)))
}

/// True when the key's resolved entry is a first-party dependency: a workspace
/// package, `npm link`, or `file:` dep whose entry path has no `node_modules`
/// segment. These change without a `NodeModulesChanged` generation bump, so they
/// bypass the TTL fast path and are re-validated on every `get`. A key that does not
/// decode to an identity returns `false`.
///
/// Relies on `normalize_identity_path` having canonicalized a symlinked workspace or
/// `npm link` dep out of `node_modules` at key-build time. If that canonicalize
/// failed, the raw symlink path is stored and the dep is misclassified as not
/// first-party; an edit is then missed for at most `REVERIFY_TTL`. Analysis read the
/// file just before, so the failure is rare, and a stored `node_modules` path cannot be
/// told apart from a genuine dependency without a new field in the key identity.
pub fn cache_key_is_first_party(key: &str) -> bool {
    decode_cache_identity(key)
        .and_then(|identity| identity.entry_path)
        .is_some_and(|entry_path| {
            !entry_path
                .split('/')
                .any(|segment| segment == "node_modules")
        })
}

fn cache_identity_for_import(
    request: &ImportRequest,
    resolved: Option<&ResolvedPackage>,
) -> CacheIdentity {
    let mut named_exports = if matches!(&request.import_kind, ImportKind::Named) {
        request.named.clone()
    } else {
        Vec::new()
    };
    named_exports.sort();
    named_exports.dedup();

    CacheIdentity {
        analyzer_version: ANALYZER_VERSION.to_owned(),
        specifier: request.specifier.clone(),
        package_name: request.package_name.clone(),
        package_version: request.version.clone(),
        package_root: resolved.map(|package| normalize_identity_path(&package.package_root)),
        entry_path: resolved.map(|package| normalize_identity_path(&package.entry_path)),
        runtime: request.runtime,
        import_kind: request.import_kind,
        named_exports,
    }
}

pub fn decode_cache_identity(key: &str) -> Option<CacheIdentity> {
    // Built once: decode runs on scan paths (invalidation, orphan checks).
    static PREFIX: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| format!("v{CACHE_KEY_VERSION}:"));
    let encoded = key.strip_prefix(PREFIX.as_str())?;
    let bytes = hex_decode(encoded)?;
    rmp_serde::from_slice(&bytes).ok()
}

/// Definitive-absence test for reclaim/delete paths. Unlike `Path::exists()`
/// (which maps every error to `false`), this returns `true` only when a stat
/// reports `NotFound`; a locked file, offline drive, or permission error returns
/// `false`, so a transient condition never destroys a valid cache.
pub fn path_is_definitely_gone(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Reachability of a project root, for the destructive orphan-shard reclaim.
/// `path_is_definitely_gone` alone is unsafe here: on Windows a released drive
/// letter reports `ERROR_PATH_NOT_FOUND`, which Rust maps to `NotFound`, so an
/// unplugged drive would look like a deleted project. An orphan therefore requires
/// that some ancestor of the root confirmably exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRootState {
    /// The root exists: keep the shard.
    Present,
    /// The root is confirmably absent and an ancestor exists: the shard may be
    /// removed.
    Orphaned,
    /// Neither the root nor any ancestor is reachable, or a stat failed: keep the
    /// shard.
    VolumeUnreachable,
}

/// Classify a project root for orphan reclaim. Only `Orphaned` authorizes a
/// destructive shard removal; both other states keep the shard.
pub fn classify_project_root(root: &Path) -> ProjectRootState {
    classify_project_root_with(root, |path| path.try_exists())
}

/// Core of [`classify_project_root`], with the existence probe injected so the
/// Windows unplugged-drive case (every ancestor reports `NotFound`) is testable.
fn classify_project_root_with(
    root: &Path,
    exists: impl Fn(&Path) -> std::io::Result<bool>,
) -> ProjectRootState {
    match exists(root) {
        Ok(true) => ProjectRootState::Present,
        // Confirmed absent (`NotFound`). Prove the volume is live before treating
        // this as a deletion: an unplugged drive reports every ancestor absent too.
        Ok(false) => {
            if root
                .ancestors()
                .skip(1)
                .any(|ancestor| matches!(exists(ancestor), Ok(true)))
            {
                ProjectRootState::Orphaned
            } else {
                ProjectRootState::VolumeUnreachable
            }
        }
        // Permission, not-ready, or other transient error: never destroy on doubt.
        Err(_) => ProjectRootState::VolumeUnreachable,
    }
}

/// Whether a cache entry is an orphan the user's purge action should drop:
/// built by a different analyzer version (release-stale), or resolved from a
/// package whose entry/root no longer exists on disk (uninstalled). A *changed*
/// file is NOT an orphan (it recomputes on access); only a *missing* one is, so
/// this checks path existence, not fingerprint currency. Undecodable keys are
/// left alone.
pub fn cache_key_is_orphan(key: &str, current_analyzer_version: &str) -> bool {
    let Some(identity) = decode_cache_identity(key) else {
        return false;
    };
    if identity.analyzer_version != current_analyzer_version {
        return true;
    }
    if identity
        .entry_path
        .as_deref()
        .is_some_and(|path| path_is_definitely_gone(Path::new(path)))
    {
        return true;
    }
    identity
        .package_root
        .as_deref()
        .is_some_and(|path| path_is_definitely_gone(Path::new(path)))
}

/// Whether `key` belongs to any package in `package_names`. Decodes the identity
/// once and tests set membership, so invalidating a burst of packages is one
/// O(keys) pass.
pub fn cache_key_matches_any_package(key: &str, package_names: &HashSet<String>) -> bool {
    if let Some(identity) = decode_cache_identity(key) {
        return package_names.contains(&identity.package_name);
    }

    // A key with no decodable identity is matched by a plaintext `name@` or `name/` prefix.
    package_names.iter().any(|package_name| {
        key.starts_with(&format!("{package_name}@")) || key.starts_with(&format!("{package_name}/"))
    })
}

/// Put fingerprint sets in deterministic cache-key order while preserving conflicting snapshots.
///
/// Two identical observations of one path collapse. Two different hashes for one path both
/// remain: no current file can satisfy both, which makes an analysis that saw a mid-flight edit
/// non-reusable. Deduplicating by path alone would pick one snapshot and could bless a size
/// derived from the other.
pub fn sort_and_dedup_fingerprints(fingerprints: &mut Vec<FileFingerprint>) {
    fingerprints.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.len.cmp(&right.len))
            .then_with(|| left.modified_millis.cmp(&right.modified_millis))
            .then_with(|| left.content_hash.cmp(&right.content_hash))
    });
    fingerprints.dedup();
}

/// Whether one analysis observed mutually incompatible snapshots for the same path.
///
/// A hashless observation is compatible with a hashed one when their metadata agrees; it simply
/// knows less. Different metadata, or two different known hashes, means the file changed while the
/// answer was being assembled. No single on-disk state can validate that answer, so it must not be
/// admitted to a cache even on the node_modules metadata fast path.
pub fn fingerprints_have_conflicting_snapshots(fingerprints: &[FileFingerprint]) -> bool {
    let mut observed: HashMap<&str, (u64, u64, Option<u64>)> = HashMap::new();
    for fingerprint in fingerprints {
        match observed.entry(&fingerprint.path) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((
                    fingerprint.len,
                    fingerprint.modified_millis,
                    fingerprint.content_hash,
                ));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let (len, modified_millis, known_hash) = entry.get_mut();
                if *len != fingerprint.len || *modified_millis != fingerprint.modified_millis {
                    return true;
                }
                match (*known_hash, fingerprint.content_hash) {
                    (Some(left), Some(right)) if left != right => return true,
                    (None, Some(hash)) => *known_hash = Some(hash),
                    _ => {}
                }
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Verified current against the file on disk.
    Fresh,
    /// A dependency file's content changed (still present).
    Stale,
    /// A dependency file is definitively absent (`NotFound`).
    Gone,
    /// Could not verify (transient stat/read error). Caller must KEEP, not evict.
    Unknown,
}

fn classify_stat_error(kind: std::io::ErrorKind) -> Freshness {
    if kind == std::io::ErrorKind::NotFound {
        Freshness::Gone
    } else {
        Freshness::Unknown
    }
}

/// Milliseconds since the Unix epoch of a file's mtime, or 0 when the platform
/// reports no mtime or a value that does not fit a `u64` of milliseconds.
fn modified_millis(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

/// Tri-state freshness of one stored fingerprint against the current file.
pub fn check_fingerprint(stored: &FileFingerprint) -> Freshness {
    // An absent input is fresh exactly while the file stays missing: a stylesheet that
    // `@import`s a nonexistent file is a deterministic fact about the package. Creating the file
    // makes this Stale, which re-measures.
    if fingerprint_is_absent(stored) {
        return match fs::metadata(&stored.path) {
            Ok(_) => Freshness::Stale,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Freshness::Fresh,
            Err(error) => classify_stat_error(error.kind()),
        };
    }

    let metadata = match fs::metadata(&stored.path) {
        Ok(metadata) => metadata,
        Err(error) => return classify_stat_error(error.kind()),
    };
    let current_len = metadata.len();
    let current_mtime = modified_millis(&metadata);

    // Cheap pre-filter: unchanged mtime+len means unchanged content, so skip the read.
    if current_len == stored.len && current_mtime == stored.modified_millis {
        return Freshness::Fresh;
    }

    // mtime/len differ. With a content hash we can tell a real change from a
    // no-op touch; without one we can only assume Stale.
    match stored.content_hash {
        Some(expected) => verify_content_hash(&stored.path, expected),
        None => Freshness::Stale,
    }
}

/// Like `check_fingerprint`, but never trusts the mtime+len pre-filter when a
/// content hash is present: it re-reads and compares the hash. Used for
/// first-party/linked source files (probed every get), where a mtime-preserving,
/// equal-length rewrite would otherwise be served stale.
pub fn check_fingerprint_strict(stored: &FileFingerprint) -> Freshness {
    match stored.content_hash {
        Some(expected) => verify_content_hash(&stored.path, expected),
        // No hash to verify: mtime+len is all there is.
        None => check_fingerprint(stored),
    }
}

/// Re-reads `path` and compares its content hash with the stored one.
fn verify_content_hash(path: &str, expected: u64) -> Freshness {
    match fs::read(path) {
        Ok(bytes) if content_hash(&bytes) == expected => Freshness::Fresh,
        Ok(_) => Freshness::Stale,
        Err(error) => classify_stat_error(error.kind()),
    }
}

/// An input under `node_modules`, which changes only through an install (a cache-generation bump)
/// and so is trusted on len+mtime. Everything else is first-party and hash-verified.
pub fn fingerprint_is_installed(fingerprint: &FileFingerprint) -> bool {
    fingerprint.path.contains("/node_modules/")
}

/// Worst-case freshness across a set, hash-verifying first-party (non-node_modules)
/// files strictly while keeping the cheap `check_fingerprint` pre-filter for
/// node_modules files (which cannot silently change without a generation bump).
/// Precedence: Unknown > Gone > Stale > Fresh, so a transient error on any file
/// never triggers a destructive decision.
pub fn check_fingerprints_strict(fingerprints: &[FileFingerprint]) -> Freshness {
    let mut worst = Freshness::Fresh;
    for fingerprint in fingerprints {
        let freshness = if fingerprint_is_installed(fingerprint) {
            check_fingerprint(fingerprint)
        } else {
            check_fingerprint_strict(fingerprint)
        };
        match freshness {
            Freshness::Unknown => return Freshness::Unknown,
            // `Stale` only upgrades `Fresh`, so it never downgrades a `Gone`.
            Freshness::Gone => worst = Freshness::Gone,
            Freshness::Stale if matches!(worst, Freshness::Fresh) => worst = Freshness::Stale,
            _ => {}
        }
    }
    worst
}

fn encode_cache_identity(identity: &CacheIdentity) -> String {
    let bytes = rmp_serde::to_vec(identity).unwrap_or_default();
    format!("v{CACHE_KEY_VERSION}:{}", hex_encode(&bytes))
}

/// Stat `path` for len+mtime and attach an already-computed content hash (from
/// the bytes read at analysis time). `content_hash: None` degrades to mtime+len.
pub fn file_fingerprint_with_hash(
    path: impl AsRef<Path>,
    content_hash: Option<u64>,
) -> Option<FileFingerprint> {
    let path = path.as_ref();
    let metadata = fs::metadata(path).ok()?;
    let modified_millis = modified_millis(&metadata);
    Some(FileFingerprint {
        path: normalize_identity_path(path),
        len: metadata.len(),
        modified_millis,
        content_hash,
    })
}

/// Read `path` now and fingerprint it with a content hash, for fallback paths (the
/// manifest, the no-graph entry) that carry no read-time hash out of analysis. The
/// hash catches a later equal-length, mtime-preserving change and ignores a no-op
/// touch. Falls back to a stat-only fingerprint if the read fails, so the file is not
/// dropped from the fingerprint set.
pub fn file_fingerprint_reading_hash(path: impl AsRef<Path>) -> Option<FileFingerprint> {
    let path = path.as_ref();
    match fs::read(path) {
        Ok(bytes) => file_fingerprint_with_hash(path, Some(content_hash(&bytes))),
        Err(_) => file_fingerprint_with_hash(path, None),
    }
}

/// Represent a path that analysis attempted but could not read.
///
/// No bytes can make this state `Fresh`. Maximal len/mtime make an accessible file miss the
/// metadata pre-filter and, with no content hash, classify Stale; a still-missing file classifies
/// Gone. Either refuses the cached fallback, keeping a machine-dependent read failure out of
/// durable use without a new variant in the serialized fingerprint schema.
pub fn unverifiable_file_fingerprint(path: impl AsRef<Path>) -> FileFingerprint {
    FileFingerprint {
        path: normalize_identity_path(path),
        len: u64::MAX,
        modified_millis: u64::MAX,
        content_hash: None,
    }
}

/// An input expected not to exist, whose continued absence keeps a result fresh.
///
/// Distinguished from [`unverifiable_file_fingerprint`] (all-ones in both fields, never fresh)
/// by a zero mtime paired with an all-ones length, a combination no real file produces.
pub fn absent_file_fingerprint(path: impl AsRef<Path>) -> FileFingerprint {
    FileFingerprint {
        path: normalize_identity_path(path),
        len: u64::MAX,
        modified_millis: 0,
        content_hash: None,
    }
}

pub fn fingerprint_is_absent(fingerprint: &FileFingerprint) -> bool {
    fingerprint.len == u64::MAX
        && fingerprint.modified_millis == 0
        && fingerprint.content_hash.is_none()
}

pub fn fingerprint_is_unverifiable(fingerprint: &FileFingerprint) -> bool {
    fingerprint.len == u64::MAX
        && fingerprint.modified_millis == u64::MAX
        && fingerprint.content_hash.is_none()
}

/// Whether a dependency set represents one complete, internally consistent observation.
pub fn fingerprints_are_reusable(fingerprints: &[FileFingerprint]) -> bool {
    !fingerprints.iter().any(fingerprint_is_unverifiable)
        && !fingerprints_have_conflicting_snapshots(fingerprints)
}

/// Len + mtime of a module read during analysis, from a stat the caller already
/// took, using the same mtime derivation as `check_fingerprint` so a later probe
/// of an unchanged file hits the `Fresh` pre-filter.
///
/// The stat MUST be the one taken *before* the bytes were read. Stat-after-read
/// records the post-edit len+mtime against a hash of the pre-edit bytes, and
/// `check_fingerprint` short-circuits to `Fresh` on a len+mtime match, so a file
/// rewritten during the read would be served from the replaced bytes forever.
/// Stat-before-read fails safe: a mismatch falls through to the hash comparison.
pub fn read_time_len_mtime_of(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.len(), modified_millis(metadata))
}

/// Build a fingerprint from values captured at analysis read-time (len+mtime from
/// the stat taken alongside the byte read, hash of those exact bytes) WITHOUT
/// re-stat'ing, for a path that is ALREADY the canonical module key (the module
/// graph keys every module by its `fs::canonicalize`d path).
///
/// Skips the `canonicalize` that `normalize_identity_path` would repeat, so a
/// non-canonical input would key the fingerprint off a path a later probe never
/// matches. Debug builds assert the round trip (a file deleted since analysis cannot
/// be canonicalized and is accepted).
pub fn file_fingerprint_from_read_time(
    canonical_path: impl AsRef<Path>,
    len: u64,
    modified_millis: u64,
    content_hash: u64,
) -> FileFingerprint {
    let path_ref = canonical_path.as_ref();
    debug_assert!(
        std::fs::canonicalize(path_ref)
            .map(|resolved| resolved.as_path() == path_ref)
            .unwrap_or(true),
        "file_fingerprint_from_read_time requires an already-canonical path, got {}",
        path_ref.display()
    );
    FileFingerprint {
        path: identity_path_string(path_ref),
        len,
        modified_millis,
        content_hash: Some(content_hash),
    }
}

fn normalize_identity_path(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    identity_path_string(&fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
}

/// The stored spelling of a path in cache keys and fingerprints, which are also
/// stat'd later. `/`-separated on Windows, where `\` is a separator; verbatim
/// elsewhere, where `\` is an ordinary file-name character and rewriting it would
/// name a different file.
pub fn identity_path_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| Some((hex_value(high)? << 4) | hex_value(low)?))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_project_root_present_when_root_exists() {
        let state = classify_project_root_with(Path::new("C:/live/app"), |_| Ok(true));
        assert_eq!(state, ProjectRootState::Present);
    }

    #[test]
    fn classify_project_root_orphaned_when_folder_gone_but_volume_live() {
        // Root absent, but its parent (the live drive) exists → genuine deletion.
        let root = Path::new("C:/live/deleted-app");
        let state = classify_project_root_with(root, |path| Ok(path != root));
        assert_eq!(state, ProjectRootState::Orphaned);
    }

    #[test]
    fn classify_project_root_keeps_shard_on_unplugged_drive() {
        // A released Windows drive letter reports ERROR_PATH_NOT_FOUND (NotFound)
        // for the root and every ancestor. The shard must be kept.
        let state = classify_project_root_with(Path::new("D:/app"), |_| Ok(false));
        assert_eq!(state, ProjectRootState::VolumeUnreachable);
    }

    #[test]
    fn classify_project_root_keeps_shard_on_stat_error() {
        let state = classify_project_root_with(Path::new("C:/locked/app"), |_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert_eq!(state, ProjectRootState::VolumeUnreachable);
    }

    #[test]
    fn content_hash_is_deterministic_and_distinguishes_content() {
        assert_eq!(
            content_hash(b"export const x = 1;"),
            content_hash(b"export const x = 1;")
        );
        assert_ne!(
            content_hash(b"export const x = 1;"),
            content_hash(b"export const x = 2;")
        );
        // Same length, different content: the case mtime+len can miss.
        assert_ne!(content_hash(b"aaaa"), content_hash(b"bbbb"));
    }

    #[test]
    fn fingerprint_normalization_keeps_conflicting_snapshots_of_one_path() {
        let first = FileFingerprint {
            path: "/pkg/styles.css".to_owned(),
            len: 4,
            modified_millis: 10,
            content_hash: Some(content_hash(b"aaaa")),
        };
        let conflicting = FileFingerprint {
            content_hash: Some(content_hash(b"bbbb")),
            ..first.clone()
        };
        let mut fingerprints = vec![conflicting.clone(), first.clone(), first.clone()];

        sort_and_dedup_fingerprints(&mut fingerprints);

        assert_eq!(fingerprints.len(), 2, "exact duplicates should collapse");
        assert!(fingerprints.contains(&first));
        assert!(fingerprints.contains(&conflicting));
        assert!(
            fingerprints_have_conflicting_snapshots(&fingerprints),
            "two known hashes for one metadata snapshot cannot both describe the answer"
        );
        assert!(!fingerprints_are_reusable(&fingerprints));

        let mut compatible = first.clone();
        compatible.content_hash = None;
        assert!(
            !fingerprints_have_conflicting_snapshots(&[first, compatible]),
            "a stat-only observation may agree with a more precise hashed observation"
        );
    }

    /// The absent sentinel is the only fingerprint kind that can return `Fresh` from a failed
    /// stat, and only for `NotFound`: a locked or permission-denied file is not evidence that the
    /// file is gone.
    #[test]
    fn an_absent_fingerprint_is_fresh_only_while_the_file_is_missing() {
        let dir = std::env::temp_dir().join(format!(
            "il-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let missing = dir.join("created-later.css");
        let fingerprint = absent_file_fingerprint(&missing);

        assert!(fingerprint_is_absent(&fingerprint));
        assert!(
            !fingerprint_is_unverifiable(&fingerprint),
            "absent and unverifiable must stay distinct: one can be fresh, the other never can"
        );
        assert_eq!(
            check_fingerprint(&fingerprint),
            Freshness::Fresh,
            "a file that is still missing is exactly what this fingerprint recorded"
        );
        assert!(
            fingerprints_are_reusable(std::slice::from_ref(&fingerprint)),
            "a deterministic absence must not refuse the result it belongs to"
        );

        std::fs::write(&missing, b".created { color: red }").expect("create the missing input");
        assert_eq!(
            check_fingerprint(&fingerprint),
            Freshness::Stale,
            "creating the file is what re-measures the package"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn identity_paths_rewrite_backslashes_only_where_they_are_separators() {
        let spelled = identity_path_string(Path::new(r"C:\ws\node_modules\x\a\b.js"));
        if cfg!(windows) {
            assert_eq!(spelled, "C:/ws/node_modules/x/a/b.js");
        } else {
            assert_eq!(spelled, r"C:\ws\node_modules\x\a\b.js");
        }
    }

    /// On POSIX `a\b.css` is one file name. Stored as `a/b.css`, the absent check would stat a
    /// path nobody creates and stay Fresh after the real file appears.
    #[cfg(unix)]
    #[test]
    fn an_absent_input_with_a_backslash_in_its_name_expires_when_created() {
        let dir = std::env::temp_dir().join(format!(
            "il-absent-backslash-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let missing = dir.join(r"a\b.css");
        let fingerprint = absent_file_fingerprint(&missing);
        assert_eq!(check_fingerprint(&fingerprint), Freshness::Fresh);

        std::fs::write(&missing, b".a { color: red }").expect("create the input");
        assert_eq!(check_fingerprint(&fingerprint), Freshness::Stale);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_unverifiable_fingerprint_can_never_be_fresh() {
        let dir = std::env::temp_dir().join(format!(
            "il-unverifiable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let missing = dir.join("created-later.css");
        let fingerprint = unverifiable_file_fingerprint(&missing);

        assert!(!fingerprints_are_reusable(std::slice::from_ref(
            &fingerprint
        )));
        assert_eq!(check_fingerprint(&fingerprint), Freshness::Gone);
        std::fs::write(&missing, b".created { color: red }").expect("create missing input");
        assert_eq!(check_fingerprint(&fingerprint), Freshness::Stale);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cache_key_is_first_party_detects_node_modules_entry() {
        let base = CacheIdentity {
            analyzer_version: ANALYZER_VERSION.to_owned(),
            specifier: "lib".to_owned(),
            package_name: "lib".to_owned(),
            package_version: "1.0.0".to_owned(),
            package_root: Some("C:/proj/node_modules/lib".to_owned()),
            entry_path: Some("//?/c:/proj/node_modules/lib/index.js".to_owned()),
            runtime: ImportRuntime::Component,
            import_kind: ImportKind::Namespace,
            named_exports: Vec::new(),
        };
        let node_modules_key = encode_cache_identity(&base);
        assert!(
            !cache_key_is_first_party(&node_modules_key),
            "a node_modules entry is not first-party"
        );

        let first_party = CacheIdentity {
            package_root: Some("C:/proj/packages/ui".to_owned()),
            entry_path: Some("//?/c:/proj/packages/ui/index.ts".to_owned()),
            ..base
        };
        let first_party_key = encode_cache_identity(&first_party);
        assert!(
            cache_key_is_first_party(&first_party_key),
            "a workspace entry (no node_modules) is first-party"
        );

        // A non-decodable key is not first-party.
        assert!(!cache_key_is_first_party("v4:not-hex"));
    }

    #[test]
    fn file_fingerprint_without_hash_serializes_byte_identical_to_pre_hash_format() {
        // A hashless fingerprint must serialize to the same msgpack bytes as a
        // 3-field struct. rmp_serde encodes structs and tuples as bare arrays, so a
        // 3-tuple of the same values is a faithful stand-in.
        let fp = FileFingerprint {
            path: "/pkg/index.js".to_string(),
            len: 42,
            modified_millis: 1_700_000_000_000,
            content_hash: None,
        };
        let legacy_equivalent = ("/pkg/index.js".to_string(), 42u64, 1_700_000_000_000u64);
        assert_eq!(
            rmp_serde::to_vec(&fp).expect("serialize fingerprint"),
            rmp_serde::to_vec(&legacy_equivalent).expect("serialize legacy tuple"),
        );

        // With a hash present the encoding grows to 4 elements, so the comparison
        // above is not vacuous.
        let fp_with_hash = FileFingerprint {
            content_hash: Some(123),
            ..fp.clone()
        };
        assert_ne!(
            rmp_serde::to_vec(&fp).expect("serialize fingerprint"),
            rmp_serde::to_vec(&fp_with_hash).expect("serialize fingerprint with hash"),
        );
    }

    #[test]
    fn classify_stat_error_only_notfound_is_gone() {
        use std::io::ErrorKind;
        assert!(matches!(
            classify_stat_error(ErrorKind::NotFound),
            Freshness::Gone
        ));
        assert!(matches!(
            classify_stat_error(ErrorKind::PermissionDenied),
            Freshness::Unknown
        ));
        // Any non-NotFound error (locked file, offline drive) is transient: keep.
        assert!(matches!(
            classify_stat_error(ErrorKind::Other),
            Freshness::Unknown
        ));
    }

    #[test]
    fn check_fingerprint_content_hash_ignores_mtime_only_touch() {
        let dir = std::env::temp_dir().join(format!(
            "il-fp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("m.js");
        std::fs::write(&file, b"export const x = 1;").expect("write");

        // Fingerprint WITH content hash of the real bytes.
        let hash = content_hash(b"export const x = 1;");
        let fp = file_fingerprint_with_hash(&file, Some(hash)).expect("fp");
        assert!(matches!(check_fingerprint(&fp), Freshness::Fresh));

        // Same content hash, different stored mtime/len: the hash wins, so a no-op
        // touch stays Fresh.
        let touched = FileFingerprint {
            modified_millis: fp.modified_millis + 5_000,
            len: fp.len + 99,
            ..fp.clone()
        };
        assert!(matches!(check_fingerprint(&touched), Freshness::Fresh));

        // Real content change: Stale. The sleep puts the rewrite's millisecond mtime
        // in a new tick; two same-length writes in one millisecond coincide on
        // len+mtime and the pre-filter returns Fresh without consulting the hash.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, b"export const x = 2;").expect("rewrite");
        assert!(matches!(check_fingerprint(&fp), Freshness::Stale));

        // Deleted: Gone.
        std::fs::remove_file(&file).expect("rm");
        assert!(matches!(check_fingerprint(&fp), Freshness::Gone));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn file_fingerprint_reading_hash_detects_mtime_preserving_change() {
        // A fallback fingerprint built with a read-time content hash catches an
        // equal-length, mtime-preserving edit; a stat-only one probes Fresh forever.
        let dir = std::env::temp_dir().join(format!(
            "il-fp-rb2-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("package.json");
        std::fs::write(&file, b"export const x = 1;").expect("write");

        let original_mtime = std::fs::metadata(&file)
            .and_then(|meta| meta.modified())
            .expect("mtime");

        let hashed = file_fingerprint_reading_hash(&file).expect("hashed fp");
        assert!(
            hashed.content_hash.is_some(),
            "read+hash must capture a content hash"
        );
        // A stat-only fingerprint of the same file, for contrast.
        let hashless = file_fingerprint_with_hash(&file, None).expect("hashless fp");

        // Equal-length rewrite with mtime restored: len+mtime identical, content differs.
        std::fs::write(&file, b"export const x = 2;").expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|handle| handle.set_modified(original_mtime))
            .expect("restore mtime");

        // The read-hashed fingerprint catches it; the hashless one is fooled.
        assert!(matches!(
            check_fingerprint_strict(&hashed),
            Freshness::Stale
        ));
        assert!(matches!(check_fingerprint(&hashless), Freshness::Fresh));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn check_fingerprints_strict_precedence_across_real_files() {
        // Empty set is Fresh.
        assert_eq!(check_fingerprints_strict(&[]), Freshness::Fresh);

        let dir = std::env::temp_dir().join(format!(
            "il-fp-prec-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");

        // A present, unchanged file: Fresh.
        let fresh_path = dir.join("fresh.js");
        let fresh_bytes: &[u8] = b"export const a = 1;";
        std::fs::write(&fresh_path, fresh_bytes).expect("write fresh");
        let fresh_fp =
            file_fingerprint_with_hash(&fresh_path, Some(content_hash(fresh_bytes))).expect("fp");

        // A file whose content later changes: Stale. The rewrite changes the length
        // so detection never depends on mtime resolution.
        let stale_path = dir.join("stale.js");
        let stale_orig: &[u8] = b"export const b = 1;";
        std::fs::write(&stale_path, stale_orig).expect("write stale");
        let stale_fp =
            file_fingerprint_with_hash(&stale_path, Some(content_hash(stale_orig))).expect("fp");

        // A file that is later deleted: Gone.
        let gone_path = dir.join("gone.js");
        let gone_bytes: &[u8] = b"export const c = 1;";
        std::fs::write(&gone_path, gone_bytes).expect("write gone");
        let gone_fp =
            file_fingerprint_with_hash(&gone_path, Some(content_hash(gone_bytes))).expect("fp");

        std::fs::write(&stale_path, b"export const b = 222222;").expect("rewrite stale");
        std::fs::remove_file(&gone_path).expect("rm gone");

        // Per-file classifications hold, so the setup is not vacuous.
        assert_eq!(check_fingerprint(&fresh_fp), Freshness::Fresh);
        assert_eq!(check_fingerprint(&stale_fp), Freshness::Stale);
        assert_eq!(check_fingerprint(&gone_fp), Freshness::Gone);

        // Precedence across a set: Gone > Stale > Fresh. Unknown has no portable
        // repro and is covered by `classify_stat_error_only_notfound_is_gone`.
        assert_eq!(
            check_fingerprints_strict(std::slice::from_ref(&fresh_fp)),
            Freshness::Fresh
        );
        assert_eq!(
            check_fingerprints_strict(&[fresh_fp.clone(), stale_fp.clone()]),
            Freshness::Stale
        );
        assert_eq!(
            check_fingerprints_strict(&[fresh_fp.clone(), gone_fp.clone()]),
            Freshness::Gone
        );
        assert_eq!(
            check_fingerprints_strict(&[stale_fp, gone_fp]),
            Freshness::Gone,
            "Gone must dominate Stale regardless of order"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn check_fingerprints_strict_catches_equal_length_rewrite_the_cheap_path_misses() {
        // A first-party file rewritten with the same length and preserved mtime
        // (`cp -p`, `rsync -a`, `tar -x`, a same-millisecond edit) is invisible to the
        // mtime+len pre-filter. The stored len+mtime are set to the real post-rewrite
        // stat while the stored hash is the pre-rewrite content's, modeling the
        // collision deterministically instead of relying on the clock.
        let dir = std::env::temp_dir().join(format!(
            "il-fp-strict-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let pkg_dir = dir.join("packages").join("ui");
        std::fs::create_dir_all(&pkg_dir).expect("dir");
        let file = pkg_dir.join("index.ts");

        let original: &[u8] = b"export const x = 1;";
        std::fs::write(&file, original).expect("write v1");
        let fp = file_fingerprint_with_hash(&file, Some(content_hash(original))).expect("fp");
        assert!(
            !fp.path.contains("/node_modules/"),
            "test setup: fixture must be first-party (no node_modules segment)"
        );

        // Equal-length, different-content rewrite.
        let rewritten: &[u8] = b"export const x = 9;";
        assert_eq!(
            original.len(),
            rewritten.len(),
            "test setup: rewrite must be equal length to model the X-7 blind spot"
        );
        std::fs::write(&file, rewritten).expect("rewrite v2");

        let new_metadata = std::fs::metadata(&file).expect("stat v2");
        let stored = FileFingerprint {
            len: new_metadata.len(),
            modified_millis: modified_millis(&new_metadata),
            ..fp.clone()
        };

        // The cheap pre-filter is fooled.
        assert_eq!(check_fingerprint(&stored), Freshness::Fresh);

        // Strict hash-verifies unconditionally, regardless of the mtime+len match.
        assert_eq!(check_fingerprint_strict(&stored), Freshness::Stale);
        assert_eq!(
            check_fingerprints_strict(std::slice::from_ref(&stored)),
            Freshness::Stale
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn check_fingerprints_strict_keeps_cheap_prefilter_for_node_modules() {
        // node_modules deps change only via install, which bumps the generation, so
        // check_fingerprints_strict routes them through the cheap pre-filter.
        let dir = std::env::temp_dir().join(format!(
            "il-fp-strict-nm-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let nm_dir = dir.join("node_modules").join("lib");
        std::fs::create_dir_all(&nm_dir).expect("dir");
        let file = nm_dir.join("index.js");

        let original: &[u8] = b"module.exports = 1;";
        std::fs::write(&file, original).expect("write v1");
        let fp = file_fingerprint_with_hash(&file, Some(content_hash(original))).expect("fp");
        assert!(
            fp.path.contains("/node_modules/"),
            "test setup: fixture must be under node_modules"
        );

        let rewritten: &[u8] = b"module.exports = 2;";
        assert_eq!(
            original.len(),
            rewritten.len(),
            "test setup: rewrite must be equal length"
        );
        std::fs::write(&file, rewritten).expect("rewrite v2");

        let new_metadata = std::fs::metadata(&file).expect("stat v2");
        let stored = FileFingerprint {
            len: new_metadata.len(),
            modified_millis: modified_millis(&new_metadata),
            ..fp.clone()
        };

        // Hash-verified directly this fingerprint is Stale, so the Fresh result below
        // comes from the routing, not from the fixture failing to change.
        assert_eq!(check_fingerprint_strict(&stored), Freshness::Stale);

        // Routed through check_fingerprints_strict, the node_modules path takes the
        // cheap pre-filter and stays Fresh.
        assert_eq!(
            check_fingerprints_strict(std::slice::from_ref(&stored)),
            Freshness::Fresh
        );

        std::fs::remove_dir_all(dir).ok();
    }
}
