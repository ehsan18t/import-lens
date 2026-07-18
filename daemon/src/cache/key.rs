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

/// Fast, non-cryptographic content hash of the bytes actually read during
/// analysis. Used to detect real content changes (and ignore no-op touches
/// such as `npm ci` that only bump mtime). Not for security.
pub fn content_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// Single source of truth for the cache-key schema version. The key prefix
/// (`v{N}:`) derives from this, so a schema bump is a one-line change here with
/// no type renames.
const CACHE_KEY_VERSION: u32 = 4;

/// The analyzer revision, in ONE place.
///
/// `ANALYZER_VERSION` has to be built with `concat!`, which only accepts literals, so
/// the revision used to be spelled twice — and bumping one without the other would
/// leave half the cache accepting entries the other half rejects. A macro keeps the
/// literal single: there is nothing left to keep in sync.
///
/// **Bump this whenever a change can alter a reported size.** Every cached entry
/// records the revision it was computed under and is rejected when it differs, which
/// is the only thing standing between a user and a size measured by the old code.
///
/// Format: `<engine>-<minor line>.x+<revision>`. The minor line (patch held as a wildcard `x`) names
/// the engine without churning on every patch — a bare `rolldown3` reads as "Rolldown v3" when the
/// crate is pinned in the 1.1.x line. A Rolldown patch that does NOT move numbers needs no edit here;
/// a patch that does — or any of our own number-moving changes — bumps the trailing `+<revision>`; a
/// minor or major bump changes the line itself (`rolldown-1.2.x+1`). Kept in step with
/// `daemon/Cargo.toml`'s pin by the `compiler-stack-upgrade` skill.
///
/// `rolldown2` (2026-07-12): post-cutover correctness fixes moved real numbers.
/// Rolldown's `//#region` debug comments are no longer billed as package cost (N2);
/// the platform is `Neutral`, so the Server runtime stopped resolving `browser`
/// export conditions and no `NODE_ENV` define is injected (N3); mixed-runtime files
/// are grouped per runtime, which was swinging a file's size by two orders of
/// magnitude depending on import order (I15); and type-position-only TypeScript
/// imports are elided (W4).
///
/// `rolldown-1.1.x+3` (2026-07-15): the release-review fixes moved numbers again, so every entry
/// computed under `rolldown2` must be rejected on read. A Windows verbatim (extended-length) entry path broke
/// Rolldown's path relativization, so a slash-bearing `sideEffects` pattern could never match and
/// side-effectful modules were dropped — `refractor` under-reported 3.7x (30 kB vs 113 kB real, 1%
/// off esbuild once fixed); array-form `sideEffects` matched with `fast_glob` now retains them
/// (Task 4). The three fabricated-size fallbacks are deleted: an unbuildable import reports no size,
/// never an invented one, and a transient failure is no longer cached (Tasks 5/6). Mixed-runtime
/// files compress per runtime and sum, ending a ~49% under-report from concatenating payloads that
/// never ship together (Task 8). The failure stage is ranked deterministically rather than decided
/// by a parse-vs-resolve race and then cached (Task 7). And export enumeration resolves under the
/// import's runtime, so an entry cached under the old hardcoded `Component` runtime no longer means
/// what it did (Task 10).
///
/// `rolldown-1.1.x+4` (2026-07-16): the release-blocker batch moves numbers again, so every entry
/// computed under `rolldown-1.1.x+3` must be rejected on read. An all-inline-`type` named import
/// (`import { type X } from "pkg"`, every specifier carrying the inline `type` keyword) was sized as a
/// namespace import of the whole package: oxc marks the entry `is_type` but leaves the module request
/// `is_type = false`, so the static-import loop dropped it while the `requested_modules` fallback
/// resurrected it as `import * as ...`. It is now registered as an elided statement and costs zero,
/// matching TypeScript's erasure (B1). And native-binary-backed packages (a platform-specific binary
/// shipped as `optionalDependencies`: Biome, TypeScript 7, esbuild) are no longer mismeasured — one
/// with no importable JS entry is answered as a native-binary-only zero instead of a bare failure, and
/// one whose JS entry is a thin shim keeps its measured size with a native-binary flag beside it — so
/// entries cached as `entry_resolution` failures or as confident shim sizes must be recomputed (B3).
///
/// `rolldown-1.1.x+5` (2026-07-17): asset counting moves the number for a whole category of packages,
/// so every entry computed under `rolldown-1.1.x+4` must be rejected on read. A package's shipped
/// CSS, wasm, and font bytes are now folded INTO the Import Cost instead of being disclosed beside a
/// number that excluded them: stylesheets are bundled and minified by Lightning CSS (one artifact per
/// import, `@import`s inlined and deduped), wasm and fonts are counted raw, and each artifact is
/// compressed on its own and summed (ADR-0005). Every CSS-shipping package therefore reports a LARGER
/// and correct size where it previously undercounted, and one whose only uncounted bytes were assets
/// can now reach High confidence, because the `uncounted_assets` diagnostic that held it at Medium is
/// emitted only when an asset cannot be processed. The result also carries a per-kind
/// `asset_breakdown`, which no `+4` entry has (B2).
///
/// `rolldown-1.1.x+6` (2026-07-18): asset measurements and freshness now use the exact same read.
/// Direct assets, CSS entrypoints, imported stylesheets, and local CSS resources are retained as
/// immutable snapshots with read-time content hashes; failed and missing CSS dependencies also
/// participate in freshness instead of leaving a reusable fallback. Combined File Cost entries
/// validate those exact fingerprints before reuse. Entries computed under `+5` can contain bytes
/// from one read paired with a later fingerprint, or remain cached after a CSS dependency changes,
/// and therefore must be rejected.
///
/// `rolldown-1.1.x+7` (2026-07-18): every asset-processing read now shares the graph's aggregate
/// resource ceiling and a bounded execution deadline. Cached `+6` measurements may include CSS
/// `@import` or `url()` resources that escaped the graph limit, so they must be recomputed under the
/// unified admission rules.
///
/// `rolldown-1.1.x+8` (2026-07-18): asset I/O and asset-compressor failures are request-local, not
/// package facts. Direct relative assets now resolve through the observing plugin, failed reads
/// retain a never-reusable fingerprint and an `asset_io` stage, and CSS/binary compressor failures
/// retain `compression`. Cached `+7` resolve failures and disclosed asset floors lack those causes
/// and could otherwise outlive the filesystem/machine condition that produced them.
/// `rolldown-1.1.x+9` (2026-07-18): a stylesheet's `url()` graph is now classified by what the
/// reference IS rather than by whether its extension is countable. A shipped file outside the
/// CSS/wasm/font taxonomy (an image, an SVG) is disclosed with its real bytes instead of leaving
/// through a silent `None`; a local resource that could not be located or whose URLs could not be
/// inspected is reported as an omission on `uncounted_assets` rather than as an over-count on
/// `imprecise_assets`; and a runtime-fetched resource is disclosed on `external`, which keeps the
/// exact size budgetable. Cached `+8` entries can therefore be short by an undisclosed image at
/// High confidence, or carry an omission mislabelled as imprecision — both of which changed
/// completeness, confidence, and budgetability — so they must be rejected on read.
/// `rolldown-1.1.x+10` (2026-07-18): two stored values changed meaning. A module whose bytes were
/// measured but whose read was never fingerprinted is now recorded as UNVERIFIABLE rather than
/// deferred to a post-analysis stat, so a `+9` entry can pair a size measured from one revision of a
/// binary module with a hash taken from a later one and answer Fresh forever. And every directly
/// imported asset is now a module contribution, so `shared_bytes` — carried in the L2 envelope —
/// accounts for a stylesheet several imports pull, which a `+9` entry computed without.
/// `rolldown-1.1.x+11` (2026-07-18): a package that a directly imported image, icon or media file
/// used to make entirely unmeasurable now measures, with those bytes disclosed. A `+10` entry for
/// such a package is a cached DURABLE failure — `resolve` for a PNG that failed the UTF-8 loader,
/// `parse` for an SVG that reached the JavaScript parser — and would keep being served as
/// "unavailable" for a package that now has a number. A bare CSS `@import` also stops recording a
/// failed read of a path that cannot exist, so results that were permanently non-durable become
/// cacheable.
/// `rolldown-1.1.x+12` (2026-07-18): brotli moved from quality 4 to quality 9, so EVERY brotli
/// figure the product reports changes. A `+11` entry was compressed at the old quality and would be
/// served beside newly measured ones, making two packages incomparable for the reason least visible
/// to the user. Measured: q4 reads 16.0% high against a CDN at q11, q9 reads 7.5% high, and the cost
/// is +33 ms per artifact against q11 own +928 ms.
macro_rules! analyzer_revision {
    () => {
        "rolldown-1.1.x+12"
    };
}

pub const ANALYZER_REVISION: &str = analyzer_revision!();
pub const ANALYZER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", analyzer_revision!());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub len: u64,
    pub modified_millis: u64,
    /// xxh3 of the bytes read during analysis. Absent for fingerprints built by
    /// a pure stat (`file_fingerprint`). Skipped when None so the serialized key
    /// stays identical to the pre-content-hash format.
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

/// True when the key's resolved entry is a first-party dependency — a workspace
/// package, `npm link`, or `file:` dep whose entry path contains no `node_modules`
/// component. These change without a `NodeModulesChanged` generation bump, so they
/// must bypass the TTL fast path and be re-validated on every `get` (D3). A key that
/// does not decode to an identity (opaque/legacy) defaults to `false`, preserving the
/// existing fast-path behavior. The identity path is normalized `/`-separated (see
/// `normalize_identity_path`).
///
/// Fail-safe window (F5 / D3 edge). Classification relies on `normalize_identity_path`
/// having `fs::canonicalize`d the entry OUT of `node_modules` at key-build time: a
/// workspace / `npm link` dep symlinked under `node_modules` canonicalizes to its real
/// first-party location, so the stored path carries no `node_modules` segment and this
/// returns `true`. The sole window where such a dep is misclassified as NOT first-party
/// is when `fs::canonicalize` FAILED at key-build time and the raw symlink path under
/// `node_modules` was stored instead of the canonical target. That is bounded and narrow:
///  - Bounded: a misclassified entry takes the TTL fast path, so a first-party edit is
///    missed for at most `REVERIFY_TTL` (30s), after which it re-verifies anyway; any
///    `NodeModulesChanged`/generation bump forces the re-check sooner.
///  - Narrow: `fs::canonicalize` only fails on an inaccessible path, but analysis has
///    just read that exact file microseconds earlier, so in practice it is present and
///    the symlink resolves out of `node_modules`.
///
/// This is documented, not "fixed", by design: a `node_modules` path that MIGHT be a
/// failed-canonicalize symlink is indistinguishable from a genuine `node_modules` dep by
/// the path string alone, so failing safe (treating it as first-party) would force the
/// strict per-`get` re-read on EVERY dependency and defeat the TTL fast path; recording
/// the canonicalize-failure signal instead would require a new field in the cache-key
/// identity (the schema embedded in the key). Neither is warranted for a window this
/// narrow and this bounded.
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
    // Built once: decode runs on scan-style paths (invalidation, orphan checks)
    // where a per-call `format!` allocation is pure churn.
    static PREFIX: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| format!("v{CACHE_KEY_VERSION}:"));
    let encoded = key.strip_prefix(PREFIX.as_str())?;
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

/// Definitive-absence test for reclaim/delete paths. Unlike `Path::exists()`
/// (which maps every error to `false`), this returns `true` only when a stat
/// reports `NotFound`; a locked file, offline drive, or permission error returns
/// `false` (keep — never destroy a valid cache on a transient condition). §4.3 / X-3 / X-4.
pub fn path_is_definitely_gone(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Reachability of a project root, for the destructive orphan-shard reclaim
/// (RB-17). `path_is_definitely_gone` alone is unsafe here: on Windows a released
/// drive letter reports `ERROR_PATH_NOT_FOUND`, which Rust maps to `NotFound` —
/// so an unplugged drive would look like a deleted project and get its cache
/// destroyed (X-3 / RB-7). This distinguishes a genuinely deleted project (its
/// volume is live but the folder is gone) from an unreachable volume by requiring
/// that *some ancestor* of the root confirmably exists before declaring an orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRootState {
    /// The root exists (or is present-but-unreadable) — keep the shard.
    Present,
    /// The root is confirmably absent AND its volume is reachable (an ancestor
    /// exists) — a genuine orphan; the shard may be removed.
    Orphaned,
    /// Neither the root nor any ancestor is reachable (offline/unplugged volume,
    /// or a stat error) — keep the shard; deletion cannot be proven.
    VolumeUnreachable,
}

/// Classify a project root for orphan reclaim. Only `Orphaned` authorizes a
/// destructive shard removal; both other states keep the shard.
pub fn classify_project_root(root: &Path) -> ProjectRootState {
    classify_project_root_with(root, |path| path.try_exists())
}

/// Core of [`classify_project_root`], with the existence probe injected so the
/// drive-safety logic — including the Windows unplugged-drive case, where every
/// ancestor reports `NotFound` and no real filesystem can reproduce it portably —
/// is deterministically testable.
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
        // Permission / not-ready / other transient error — never destroy on doubt.
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

/// Whether `key` belongs to any package in `package_names`. Decodes the key's
/// identity exactly once and tests set membership, so invalidating a burst of
/// packages is a single O(keys) pass instead of O(keys * packages) with a full
/// hex+msgpack decode per (key, package).
pub fn cache_key_matches_any_package(key: &str, package_names: &HashSet<String>) -> bool {
    if let Some(identity) = decode_cache_identity(key) {
        return package_names.contains(&identity.package_name);
    }

    // Legacy non-v3 keys carry the package name as a plaintext prefix.
    package_names.iter().any(|package_name| {
        key.starts_with(&format!("{package_name}@")) || key.starts_with(&format!("{package_name}/"))
    })
}

pub fn fingerprints_for_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<FileFingerprint> {
    let mut fingerprints = paths
        .into_iter()
        .filter_map(file_fingerprint)
        .collect::<Vec<_>>();
    sort_and_dedup_fingerprints(&mut fingerprints);
    fingerprints
}

/// Put fingerprint sets in deterministic cache-key order while preserving conflicting snapshots.
///
/// Two identical observations of one path collapse. Two different hashes for one path must both
/// remain: no current file can satisfy both, so their coexistence safely makes an analysis that saw
/// a mid-flight edit non-reusable. Deduplicating only by path silently chose one snapshot and could
/// bless a size derived from the other.
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
    // An ABSENT input is the one case where "the file is not there" is the expected answer rather
    // than a reason to give up. A stylesheet that `@import`s a file which does not exist is a
    // deterministic fact about the package, so the result is worth keeping — but only for exactly
    // as long as the file stays missing. Creating it makes this Stale, which is what re-measures.
    //
    // Without this the same path had to be recorded as UNVERIFIABLE, which is never fresh, so such a
    // package was rebuilt on every keystroke over a file nobody was going to create.
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

    // Cheap pre-filter: unchanged mtime+len means unchanged content — skip the read.
    if current_len == stored.len && current_mtime == stored.modified_millis {
        return Freshness::Fresh;
    }

    // mtime/len differ. With a content hash we can tell a real change from a
    // no-op touch; without one we can only assume Stale.
    let Some(expected) = stored.content_hash else {
        return Freshness::Stale;
    };
    match fs::read(&stored.path) {
        Ok(bytes) if content_hash(&bytes) == expected => Freshness::Fresh,
        Ok(_) => Freshness::Stale,
        Err(error) => classify_stat_error(error.kind()),
    }
}

/// Worst-case freshness across a set. `Unknown` dominates so a transient error on
/// any file never triggers a destructive decision; otherwise Gone, then Stale.
pub fn check_fingerprints(fingerprints: &[FileFingerprint]) -> Freshness {
    let mut worst = Freshness::Fresh;
    for fingerprint in fingerprints {
        match check_fingerprint(fingerprint) {
            Freshness::Unknown => return Freshness::Unknown,
            // The `Unknown` arm returns early, so `worst` is never `Unknown` here;
            // and the `Stale` arm below only upgrades `Fresh`, so it can never
            // downgrade a `Gone`. Precedence stays Unknown > Gone > Stale > Fresh.
            Freshness::Gone => worst = Freshness::Gone,
            Freshness::Stale if matches!(worst, Freshness::Fresh) => worst = Freshness::Stale,
            _ => {}
        }
    }
    worst
}

/// Like `check_fingerprint`, but never trusts the mtime+len pre-filter when a
/// content hash is present: it re-reads and compares the hash. Used for
/// first-party/linked source files (probed every get), where a mtime-preserving,
/// equal-length rewrite would otherwise be served stale (X-7).
pub fn check_fingerprint_strict(stored: &FileFingerprint) -> Freshness {
    let Some(expected) = stored.content_hash else {
        return check_fingerprint(stored); // no hash to verify — mtime+len is all we have
    };
    match fs::read(&stored.path) {
        Ok(bytes) if content_hash(&bytes) == expected => Freshness::Fresh,
        Ok(_) => Freshness::Stale,
        Err(error) => classify_stat_error(error.kind()),
    }
}

/// Worst-case freshness across a set, hash-verifying first-party (non-node_modules)
/// files strictly while keeping the cheap `check_fingerprint` pre-filter for
/// node_modules files (which cannot silently change without a generation bump).
/// Same precedence as `check_fingerprints`: Unknown > Gone > Stale > Fresh.
pub fn check_fingerprints_strict(fingerprints: &[FileFingerprint]) -> Freshness {
    let mut worst = Freshness::Fresh;
    for fingerprint in fingerprints {
        let freshness = if fingerprint.path.contains("/node_modules/") {
            check_fingerprint(fingerprint)
        } else {
            check_fingerprint_strict(fingerprint)
        };
        match freshness {
            Freshness::Unknown => return Freshness::Unknown,
            Freshness::Gone => worst = Freshness::Gone,
            Freshness::Stale if matches!(worst, Freshness::Fresh) => worst = Freshness::Stale,
            _ => {}
        }
    }
    worst
}

/// Back-compatible boolean: true only when every fingerprint is `Fresh`.
pub fn fingerprints_are_current(fingerprints: &[FileFingerprint]) -> bool {
    matches!(check_fingerprints(fingerprints), Freshness::Fresh)
}

fn encode_cache_identity(identity: &CacheIdentity) -> String {
    let bytes = rmp_serde::to_vec(identity).unwrap_or_default();
    format!("v{CACHE_KEY_VERSION}:{}", hex_encode(&bytes))
}

fn file_fingerprint(path: impl AsRef<Path>) -> Option<FileFingerprint> {
    file_fingerprint_with_hash(path, None)
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

/// Read `path` NOW and fingerprint it WITH a content hash — for fallback paths
/// (the manifest, the no-graph entry) that carry no read-time hash threaded out of
/// analysis. Per §4.2 the hash IS the read: with it, a later equal-length,
/// mtime-preserving change is detected instead of silently probing Fresh (RB-2 /
/// X-1), and a no-op mtime touch (same content) no longer forces a re-verify.
/// Falls back to a stat-only fingerprint if the read fails (locked/permission) so
/// the file is not dropped from the fingerprint set.
pub fn file_fingerprint_reading_hash(path: impl AsRef<Path>) -> Option<FileFingerprint> {
    let path = path.as_ref();
    match fs::read(path) {
        Ok(bytes) => file_fingerprint_with_hash(path, Some(content_hash(&bytes))),
        Err(_) => file_fingerprint_with_hash(path, None),
    }
}

/// Represent a path that analysis attempted but could not read.
///
/// There are no bytes whose hash could make this state `Fresh`. Maximal len/mtime values guarantee
/// an accessible file misses the metadata pre-filter and, with no content hash, classifies Stale;
/// a still-missing file classifies Gone. Either outcome refuses the cached fallback, which keeps a
/// machine-dependent read failure out of durable use without changing the serialized fingerprint
/// schema to add an absence variant.
pub fn unverifiable_file_fingerprint(path: impl AsRef<Path>) -> FileFingerprint {
    FileFingerprint {
        path: normalize_identity_path(path),
        len: u64::MAX,
        modified_millis: u64::MAX,
        content_hash: None,
    }
}

/// An input that is expected NOT to exist, and whose continued absence is what keeps a result fresh.
///
/// Distinguished from [`unverifiable_file_fingerprint`] by the mtime: that one is all-ones in both
/// fields and can never be fresh, this one pairs an all-ones length with a zero mtime, a combination
/// no real file produces. Both are sentinels rather than measurements; only this one has a state a
/// later check can confirm.
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

/// Len + mtime captured at the moment a module's bytes are read during analysis,
/// using the same mtime derivation as `check_fingerprint` so a later probe of an
/// unchanged file hits the `Fresh` pre-filter. Returns `(len, modified_millis)`;
/// falls back to `(0, 0)` when the stat fails — the caller already holds the bytes,
/// so a missed stat only weakens the pre-filter, never correctness (the content
/// hash still decides).
pub fn read_time_len_mtime(path: impl AsRef<Path>) -> (u64, u64) {
    match fs::metadata(path.as_ref()) {
        Ok(metadata) => read_time_len_mtime_of(&metadata),
        Err(_) => (0, 0),
    }
}

/// Same, from a stat the caller already took.
///
/// The stat MUST be the one taken *before* the bytes were read. Stat-after-read
/// records the post-edit len+mtime against a hash of the pre-edit bytes, and
/// `check_fingerprint` short-circuits to `Fresh` on a len+mtime match without
/// hashing — so a file rewritten during the read would be served from the bytes
/// that were replaced, forever. Stat-before-read fails safe: a mismatch merely
/// falls through to the hash comparison, which is correct in both directions.
pub fn read_time_len_mtime_of(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.len(), modified_millis(metadata))
}

/// Build a fingerprint from values captured at analysis read-time (len+mtime from
/// the stat taken alongside the byte read, hash of those exact bytes) WITHOUT
/// re-stat'ing, for a path that is ALREADY the canonical module key (the module
/// graph keys every module by its `fs::canonicalize`d path).
///
/// `normalize_identity_path` would re-`canonicalize` that path — an idempotent no-op
/// on an already-canonical path — so this skips the syscall and applies only the
/// `\` → `/` identity-path normalization directly. The result is byte-identical to
/// `normalize_identity_path(canonical_path)`: on an already-canonical existing path
/// `fs::canonicalize` returns it unchanged, and if the file has since disappeared
/// `normalize_identity_path` falls back to that same raw path we forward-slash here.
/// An unchanged file still matches the pre-filter, and a file changed *after* analysis
/// yields a mismatched len/mtime (closing the post-analysis TOCTOU window).
pub fn file_fingerprint_from_read_time(
    canonical_path: impl AsRef<Path>,
    len: u64,
    modified_millis: u64,
    content_hash: u64,
) -> FileFingerprint {
    let path_ref = canonical_path.as_ref();
    // C8 precondition: `path_ref` MUST already be canonical (the module graph's
    // `fs::canonicalize`d module key). This fn deliberately skips the canonicalize
    // syscall and only forward-slashes the path, so a non-canonical input would key the
    // fingerprint off a path a later probe's canonical path never matches. `canonicalize`
    // is idempotent on a canonical path, so in debug/test builds assert the input
    // round-trips; a file deleted since analysis (canonicalize errors) is accepted
    // (`unwrap_or(true)`) — the fingerprint then falls back to the raw path exactly as
    // documented above. Compiles out entirely in release.
    debug_assert!(
        std::fs::canonicalize(path_ref)
            .map(|resolved| resolved.as_path() == path_ref)
            .unwrap_or(true),
        "file_fingerprint_from_read_time requires an already-canonical path, got {}",
        path_ref.display()
    );
    FileFingerprint {
        path: path_ref.to_string_lossy().replace('\\', "/"),
        len,
        modified_millis,
        content_hash: Some(content_hash),
    }
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
        // RB-7 / X-3 regression: a released Windows drive letter reports
        // ERROR_PATH_NOT_FOUND (→ NotFound) for the root AND every ancestor.
        // The shard must be KEPT, never destroyed.
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
        // Same length, different content — the case mtime+len can miss.
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

    /// The absent sentinel is the ONE case where "the file is not there" is the expected answer
    /// rather than a reason to give up, so all three arms of that branch are pinned here.
    ///
    /// This is the only fingerprint kind that can return `Fresh` from a failed stat. That makes its
    /// third arm — a stat that fails for any reason OTHER than absence — the one worth stating
    /// explicitly: a locked or permission-denied file is not evidence that the file is gone, and
    /// answering `Fresh` there would serve a result whose input we cannot see.
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

        // An opaque/non-decodable key defaults to NOT first-party, preserving the
        // existing fast-path behavior for anything that is not a real identity.
        assert!(!cache_key_is_first_party("v4:not-hex"));
    }

    #[test]
    fn file_fingerprint_without_hash_serializes_byte_identical_to_pre_hash_format() {
        // The single most important invariant of this change: a hashless
        // fingerprint (content_hash: None) must serialize to the exact same
        // msgpack bytes as the pre-Task-2 3-field struct, since FileFingerprint
        // is embedded in the cache KEY, not just the value. rmp_serde encodes
        // both structs and tuples as bare arrays (no field names), so a 3-tuple
        // of the same three field values is a faithful stand-in for "the old
        // 3-field struct" and must byte-match.
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

        // With a hash present, the encoding must differ (grows to 4 elements) —
        // guards against a vacuously-true comparison above.
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
        // Any non-NotFound (locked file, offline drive) is transient → keep.
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

        // Rewrite identical content but force a NEW mtime+len signature by lying in
        // the stored fingerprint: same content hash, stale mtime/len. Content hash
        // wins → still Fresh (no-op touch is not a change).
        let touched = FileFingerprint {
            modified_millis: fp.modified_millis + 5_000,
            len: fp.len + 99,
            ..fp.clone()
        };
        assert!(matches!(check_fingerprint(&touched), Freshness::Fresh));

        // Real content change → Stale. Sleep first so the rewrite's mtime
        // (truncated to milliseconds by our fingerprint) is guaranteed to land
        // in a new tick: without this, two same-length writes completing within
        // the same millisecond coincide on len+mtime and the cheap pre-filter
        // returns Fresh without ever consulting the content hash (observed
        // flaky ~50% of runs on a fast NVMe/NTFS setup without the sleep).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, b"export const x = 2;").expect("rewrite");
        assert!(matches!(check_fingerprint(&fp), Freshness::Stale));

        // Deleted → Gone.
        std::fs::remove_file(&file).expect("rm");
        assert!(matches!(check_fingerprint(&fp), Freshness::Gone));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn file_fingerprint_reading_hash_detects_mtime_preserving_change() {
        // RB-2 / X-1: a fallback fingerprint (manifest / no-graph entry) built with
        // a read-time content hash must catch an equal-length, mtime-preserving edit
        // — the stat-only (hashless) fingerprint it replaces probes Fresh forever.
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
        // The OLD behavior, for contrast: a stat-only fingerprint of the same file.
        let hashless = file_fingerprint_with_hash(&file, None).expect("hashless fp");

        // Equal-length rewrite, mtime restored → len+mtime identical, content differs.
        std::fs::write(&file, b"export const x = 2;").expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|handle| handle.set_modified(original_mtime))
            .expect("restore mtime");

        // The read-hashed fingerprint catches it; the hashless one is fooled (bug).
        assert!(matches!(
            check_fingerprint_strict(&hashed),
            Freshness::Stale
        ));
        assert!(matches!(check_fingerprint(&hashless), Freshness::Fresh));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn check_fingerprints_precedence_across_real_files() {
        // Empty set is Fresh.
        assert_eq!(check_fingerprints(&[]), Freshness::Fresh);

        let dir = std::env::temp_dir().join(format!(
            "il-fp-prec-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");

        // A present, unchanged file → Fresh. Captured with its real content hash.
        let fresh_path = dir.join("fresh.js");
        let fresh_bytes: &[u8] = b"export const a = 1;";
        std::fs::write(&fresh_path, fresh_bytes).expect("write fresh");
        let fresh_fp =
            file_fingerprint_with_hash(&fresh_path, Some(content_hash(fresh_bytes))).expect("fp");

        // A file whose content later changes → Stale. The rewrite changes the
        // LENGTH so detection never depends on mtime resolution, and the stored
        // fingerprint still carries the ORIGINAL content hash so Stale-vs-Fresh is
        // unambiguous (the content-hash read confirms the change).
        let stale_path = dir.join("stale.js");
        let stale_orig: &[u8] = b"export const b = 1;";
        std::fs::write(&stale_path, stale_orig).expect("write stale");
        let stale_fp =
            file_fingerprint_with_hash(&stale_path, Some(content_hash(stale_orig))).expect("fp");

        // A file that is later deleted → Gone.
        let gone_path = dir.join("gone.js");
        let gone_bytes: &[u8] = b"export const c = 1;";
        std::fs::write(&gone_path, gone_bytes).expect("write gone");
        let gone_fp =
            file_fingerprint_with_hash(&gone_path, Some(content_hash(gone_bytes))).expect("fp");

        // Apply the mutations the fingerprints are meant to detect.
        std::fs::write(&stale_path, b"export const b = 222222;").expect("rewrite stale");
        std::fs::remove_file(&gone_path).expect("rm gone");

        // Per-file classifications hold (proves the setup is non-vacuous).
        assert_eq!(check_fingerprint(&fresh_fp), Freshness::Fresh);
        assert_eq!(check_fingerprint(&stale_fp), Freshness::Stale);
        assert_eq!(check_fingerprint(&gone_fp), Freshness::Gone);

        // Precedence across a set: Gone > Stale > Fresh (Unknown has no portable
        // Windows repro, so it is covered only by the empty-set + unit cases).
        assert_eq!(
            check_fingerprints(std::slice::from_ref(&fresh_fp)),
            Freshness::Fresh
        );
        assert_eq!(
            check_fingerprints(&[fresh_fp.clone(), stale_fp.clone()]),
            Freshness::Stale
        );
        assert_eq!(
            check_fingerprints(&[fresh_fp.clone(), gone_fp.clone()]),
            Freshness::Gone
        );
        assert_eq!(
            check_fingerprints(&[stale_fp, gone_fp]),
            Freshness::Gone,
            "Gone must dominate Stale regardless of order"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn check_fingerprints_strict_catches_equal_length_rewrite_the_cheap_path_misses() {
        // X-7: a first-party source file rewritten with the SAME length (`cp -p`,
        // `rsync -a`, `tar -x`, codegen, or a same-millisecond edit can also preserve
        // mtime) is invisible to the mtime+len pre-filter. Rather than rely on hitting
        // the same real mtime tick (flaky — see the sleep note on
        // `check_fingerprint_content_hash_ignores_mtime_only_touch` above), this
        // fabricates the stored fingerprint the same way that test does: the STORED
        // len+mtime are forced to equal the file's ACTUAL post-rewrite stat, while the
        // stored hash is the PRE-rewrite content's hash — deterministically modeling
        // the collision instead of gambling on the clock.
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

        // Equal-length, different-content rewrite — the exact case mtime+len can't see.
        let rewritten: &[u8] = b"export const x = 9;";
        assert_eq!(
            original.len(),
            rewritten.len(),
            "test setup: rewrite must be equal length to model the X-7 blind spot"
        );
        std::fs::write(&file, rewritten).expect("rewrite v2");

        // Force the STORED fp's len+mtime to match the file's real post-rewrite stat,
        // while its content_hash still reflects the ORIGINAL bytes — exactly what an
        // undetectable-by-stat rewrite leaves behind in the stored fingerprint.
        let new_metadata = std::fs::metadata(&file).expect("stat v2");
        let stored = FileFingerprint {
            len: new_metadata.len(),
            modified_millis: modified_millis(&new_metadata),
            ..fp.clone()
        };

        // The cheap pre-filter is fooled — proves the blind spot is real here.
        assert_eq!(check_fingerprint(&stored), Freshness::Fresh);
        assert_eq!(
            check_fingerprints(std::slice::from_ref(&stored)),
            Freshness::Fresh
        );

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
        // node_modules deps cannot silently change (only via install, which bumps the
        // generation), so check_fingerprints_strict must route them through the cheap
        // check_fingerprint pre-filter — never pay for a hash read on this hot path.
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

        // Sanity: hash-verified directly (bypassing the node_modules routing), this
        // fingerprint IS Stale — so the Fresh result below comes from the routing
        // choice, not from the fixture failing to change.
        assert_eq!(check_fingerprint_strict(&stored), Freshness::Stale);

        // Routed through check_fingerprints_strict, the node_modules path takes the
        // cheap pre-filter instead and stays Fresh (perf preserved).
        assert_eq!(
            check_fingerprints_strict(std::slice::from_ref(&stored)),
            Freshness::Fresh
        );

        std::fs::remove_dir_all(dir).ok();
    }
}
