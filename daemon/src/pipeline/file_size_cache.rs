use crate::{
    cache::{
        key::{
            Freshness, cache_key_for_resolved_import, check_fingerprints_strict,
            path_is_definitely_gone,
        },
        memory::cache_generation,
    },
    engine::dependency_paths::cached_loaded_paths,
    ipc::protocol::{ImportRequest, ImportRuntime},
    pipeline::{
        analyze::AnalysisContext,
        file_size::{FileSizeComputation, SizedImport, SizedPackage},
        resolver::{ResolvedPackage, resolve_package_entry},
    },
};
use papaya::HashMap;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

// One aggregate-size entry per document path. Editing a file overwrites its
// single slot in place, so repeated edits never accumulate orphaned entries.
// Distinct files are bounded by LRU eviction.
const MAX_CACHED_FILE_SIZES: usize = 64;

// Bound how long an aggregate is served without any freshness signal. Per import
// the signature folds the package's manifest plus a content token: for node_modules
// imports just the entry stat, for first-party imports a stat per cached loaded
// path (see `resolved_import_token`). It never re-reads the transitive bundle. A
// node_modules content change with no watcher event (e.g. a watcher-excluded
// folder), or a first-party deep edit while that package's loaded paths are not
// cached (the fallback stats only the entry), is reflected in the L2 per-import
// cache after its own re-verify window but would otherwise never reach L1. This
// TTL gives L1 the same backstop as `memory::REVERIFY_TTL`, capping staleness to
// one window.
const REVERIFY_TTL_MS: u64 = 30_000;

#[derive(Debug)]
struct CachedFileSize {
    signature: u64,
    computation: FileSizeComputation,
    computed_at_millis: u64,
    last_used_millis: AtomicU64,
}

fn within_ttl(computed_at_millis: u64, now_millis: u64) -> bool {
    now_millis.saturating_sub(computed_at_millis) <= REVERIFY_TTL_MS
}

#[derive(Debug, Default)]
pub struct FileSizeCache {
    entries: HashMap<PathBuf, CachedFileSize>,
}

impl FileSizeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path, signature: u64) -> Option<FileSizeComputation> {
        let pinned = self.entries.pin();
        let entry = pinned.get(path)?;
        let now = crate::time::unix_millis_now();
        if entry.signature != signature || !within_ttl(entry.computed_at_millis, now) {
            return None;
        }
        if check_fingerprints_strict(&entry.computation.dependency_fingerprints) != Freshness::Fresh
        {
            return None;
        }
        entry.last_used_millis.store(now, Ordering::Relaxed);
        Some(entry.computation.clone())
    }

    /// Store a file's totals — **if they are the file's totals**.
    ///
    /// The gate is here, in the store (ADR-0006, invariants 3 and 4). A floor — a total missing an
    /// import that was Loading or Unmeasured — is a real number and not this file's, and a 30-second
    /// TTL is long enough for it to become the file's reported size, its persisted baseline, and its
    /// CI verdict. Refusing at the insert means a future caller cannot reintroduce that by
    /// forgetting a predicate.
    pub fn insert(&self, path: PathBuf, signature: u64, computation: FileSizeComputation) {
        if !computation.is_cacheable() {
            crate::logging::log_debug(
                "file_size_cache",
                format!(
                    "refusing to cache a non-measurement for {} (incomplete: {})",
                    path.display(),
                    computation.incomplete
                ),
            );
            return;
        }

        let pinned = self.entries.pin();
        let now = crate::time::unix_millis_now();
        pinned.insert(
            path,
            CachedFileSize {
                signature,
                computation,
                computed_at_millis: now,
                last_used_millis: AtomicU64::new(now),
            },
        );

        // Bound files-opened-then-closed by evicting the least-recently-used
        // entry. Editing the same file never triggers this because it
        // overwrites one slot rather than adding a new key.
        if pinned.len() > MAX_CACHED_FILE_SIZES
            && let Some(oldest) = pinned
                .iter()
                .min_by_key(|(_, cached)| cached.last_used_millis.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone())
        {
            pinned.remove(&oldest);
        }
    }

    /// Signature-independent presence check. Integration tests use it so a
    /// concurrent `cache_generation` bump cannot make the assertion flaky.
    pub fn contains_path(&self, path: &Path) -> bool {
        self.entries.pin().get(path).is_some()
    }

    /// Drops every cached aggregate. Called when the user clears caches so the
    /// status-bar size recomputes fresh rather than serving a memory-only entry.
    pub fn clear(&self) {
        self.entries.pin().clear();
    }

    /// Drops entries whose document path no longer exists on disk. Used by the
    /// orphan purge, which removes no shards for a deleted-file case and so would
    /// otherwise leave the aggregate cached. Returns the number removed.
    pub fn purge_missing_paths(&self) -> usize {
        let pinned = self.entries.pin();
        let missing = pinned
            .iter()
            .filter(|(path, _)| path_is_definitely_gone(path))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in &missing {
            pinned.remove(path);
        }
        missing.len()
    }

    pub fn len(&self) -> usize {
        self.entries.pin().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static SHARED_FILE_SIZE_CACHE: OnceLock<FileSizeCache> = OnceLock::new();

/// Process-wide L1 cache instance used by the file-size handlers.
pub fn shared_file_size_cache() -> &'static FileSizeCache {
    SHARED_FILE_SIZE_CACHE.get_or_init(FileSizeCache::new)
}

/// Freshness key for an L1 file-size entry: sorted per-import freshness tokens (see
/// `resolved_import_token`) plus the cache generation, folded once. Unresolvable
/// requests contribute a stable request-shape token. Sorting makes it
/// order-independent, matching `compute_file_size` which combines all imports
/// regardless of order.
pub fn file_size_signature(context: &AnalysisContext, imports: &[SizedImport]) -> u64 {
    let mut tokens = imports
        .iter()
        .map(|import| {
            // An import with no request has no resolved entry to fingerprint, and it still belongs
            // in the signature. The two kinds get DIFFERENT tokens, because they mean opposite
            // things to the total and the user can move an import between them: a package that is
            // not installed makes the total a floor (FR-024a) — installing it, or adding the
            // tsconfig `paths` entry that makes the daemon see a specifier as first-party, must move
            // the signature so the total is recomputed rather than served from L1.
            let request = match &import.package {
                SizedPackage::Installed(request) => request,
                SizedPackage::NotInstalled => {
                    return format!("not_installed:{}", import.specifier);
                }
                SizedPackage::PathAlias => return format!("path_alias:{}", import.specifier),
            };

            match resolve_package_entry(&context.active_document_path, request) {
                Ok(resolved) => resolved_import_token(request, &resolved),
                Err(_) => format!(
                    "unresolved:{}:{}:{}:{:?}:{:?}:{}",
                    request.package_name,
                    request.specifier,
                    request.version,
                    request.runtime,
                    request.import_kind,
                    request.named.join(",")
                ),
            }
        })
        .collect::<Vec<_>>();
    tokens.sort();

    let mut hasher = DefaultHasher::new();
    // Generation folds node_modules invalidation into the signature so a
    // watcher-driven bump forces recompute even when file mtimes are unchanged.
    cache_generation().hash(&mut hasher);
    for token in &tokens {
        token.hash(&mut hasher);
    }
    hasher.finish()
}

/// Per-import freshness token folded into the L1 signature.
///
/// The cache key is fingerprint-free (identity is pure), so an independent stat
/// token carries the edit signal without depending on the key. A raw len+mtime stat
/// holds all the freshness signal a full `FileFingerprint` would here, without its
/// `fs::canonicalize` (which opens the file on Windows) — this runs per import per
/// poll BEFORE the L1 hit check, so it must stay stat-only: never read contents,
/// never trigger a graph build.
///
/// A first-party package (workspace / `file:` / npm-link — a resolved entry with no
/// `node_modules` segment) is fully editable, so a deep, transitively-imported module
/// edit must move the signature. A node_modules package changes only via install,
/// which bumps `cache_generation` (folded once by the caller), so it keeps the cheap
/// entry+manifest stat and never pays to enumerate its internal modules.
fn resolved_import_token(request: &ImportRequest, resolved: &ResolvedPackage) -> String {
    let key = cache_key_for_resolved_import(request, resolved);
    let manifest_token = stat_token(&resolved.package_root.join("package.json"));
    let content_token = if path_has_node_modules_segment(&resolved.entry_path) {
        stat_token(&resolved.entry_path)
    } else {
        first_party_module_token(&resolved.entry_path, request.runtime)
    };
    format!("{key}|{content_token}|{manifest_token}")
}

/// Stat token covering every first-party path loaded by the latest engine build.
/// A deep-module edit — which changes neither the cache key nor the entry stat — moves
/// the L1 signature. This index lookup never builds. Any
/// `node_modules` modules a first-party package pulls in are skipped: they invalidate
/// via `cache_generation`, not mtime, and re-stat'ing them every poll is the cost the
/// node_modules branch deliberately avoids. With nothing cached yet, falls back to the
/// entry stat alone; a later poll, once L2 has populated the dependency-path index,
/// upgrades to full coverage. The tokens are sorted so the result is stable regardless
/// of module order.
fn first_party_module_token(entry_path: &Path, runtime: ImportRuntime) -> String {
    let module_paths = cached_loaded_paths(entry_path, runtime);
    let Some(module_paths) = module_paths else {
        return stat_token(entry_path);
    };
    let mut tokens = module_paths
        .iter()
        .filter(|path| !path_has_node_modules_segment(path))
        .map(|path| {
            format!(
                "{}={}",
                path.to_string_lossy().replace('\\', "/"),
                stat_token(path)
            )
        })
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.join(",")
}

/// Whether any component of `path` is a `node_modules` directory. Mirrors
/// `key::cache_key_is_first_party`'s notion (a resolved entry with no `node_modules`
/// segment is first-party) but tests the live `Path` directly, sparing a cache-key
/// decode per import per poll.
fn path_has_node_modules_segment(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, std::path::Component::Normal(name) if name == "node_modules")
    })
}

/// Cheap edit-sensitivity token for one file: len + mtime from a single stat.
/// A missing/unstatable file yields a distinct constant so its transition into
/// or out of existence also changes the signature.
fn stat_token(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified_millis = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default();
            format!("{}:{modified_millis}", metadata.len())
        }
        Err(_) => "absent".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::memory::bump_cache_generation;
    use crate::ipc::protocol::{ImportKind, ImportRuntime};

    /// `file_size_signature` folds the process-global cache generation, so every test here that
    /// compares two signatures — and the one that deliberately bumps it — must serialize against
    /// the rest of the binary. See `cache::memory::hold_cache_generation_steady`.
    use crate::cache::memory::hold_cache_generation_steady as hold_generation_steady;

    fn computation(minified: u64) -> FileSizeComputation {
        FileSizeComputation {
            minified_bytes: minified,
            ..FileSizeComputation::default()
        }
    }

    /// An import whose own size the caller already knows nothing about; the signature only
    /// reads the request, so the measurement is irrelevant here.
    fn sized(request: ImportRequest) -> SizedImport {
        SizedImport::installed(request, None)
    }

    #[test]
    fn get_on_empty_is_none() {
        assert!(
            FileSizeCache::new()
                .get(Path::new("/a/index.ts"), 1)
                .is_none()
        );
    }

    #[test]
    fn insert_then_get_with_matching_signature_returns_value() {
        let cache = FileSizeCache::new();
        cache.insert(PathBuf::from("/a/index.ts"), 42, computation(1234));
        assert_eq!(
            cache
                .get(Path::new("/a/index.ts"), 42)
                .expect("matching signature should hit")
                .minified_bytes,
            1234
        );
        assert!(cache.contains_path(Path::new("/a/index.ts")));
        assert!(!cache.contains_path(Path::new("/a/other.ts")));
    }

    #[test]
    fn get_with_stale_signature_is_none() {
        let cache = FileSizeCache::new();
        cache.insert(PathBuf::from("/a/index.ts"), 42, computation(1234));
        assert!(cache.get(Path::new("/a/index.ts"), 99).is_none());
    }

    #[test]
    fn get_revalidates_exact_first_party_dependency_bytes() {
        let root = std::env::temp_dir().join(format!(
            "il-fsc-exact-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).expect("fixture directory");
        let dependency = root.join("asset.woff2");
        std::fs::write(&dependency, b"old-bytes").expect("old dependency bytes");
        let modified = std::fs::metadata(&dependency)
            .and_then(|metadata| metadata.modified())
            .expect("dependency mtime");
        let fingerprint = crate::cache::key::file_fingerprint_reading_hash(&dependency)
            .expect("dependency fingerprint");

        let mut value = computation(1234);
        value.dependency_fingerprints.push(fingerprint);
        let document = root.join("index.ts");
        let cache = FileSizeCache::new();
        cache.insert(document.clone(), 42, value);
        assert!(cache.get(&document, 42).is_some(), "unchanged input hits");

        // Same length and restored mtime defeat a stat-only token. A first-party combined-build
        // fingerprint still hash-verifies and must reject the cached aggregate.
        std::fs::write(&dependency, b"new-bytes").expect("new dependency bytes");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&dependency)
            .expect("dependency handle");
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("restore dependency mtime");

        assert!(
            cache.get(&document, 42).is_none(),
            "equal-length, mtime-preserving asset edits must invalidate File Cost"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reinserting_same_path_overwrites_in_place_without_orphans() {
        let cache = FileSizeCache::new();
        cache.insert(PathBuf::from("/a/index.ts"), 1, computation(10));
        cache.insert(PathBuf::from("/a/index.ts"), 2, computation(20));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(Path::new("/a/index.ts"), 1).is_none());
        assert_eq!(
            cache
                .get(Path::new("/a/index.ts"), 2)
                .expect("newest signature should hit")
                .minified_bytes,
            20
        );
    }

    #[test]
    fn purge_missing_paths_drops_entries_for_deleted_documents() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("il-fsc-purge-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let present = dir.join("present.ts");
        fs::write(&present, "x").expect("write");
        let missing = dir.join("missing.ts");

        let cache = FileSizeCache::new();
        cache.insert(present.clone(), 1, computation(10));
        cache.insert(missing.clone(), 1, computation(20));

        assert_eq!(cache.purge_missing_paths(), 1);
        assert!(cache.contains_path(&present));
        assert!(!cache.contains_path(&missing));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn within_ttl_bounds_staleness() {
        assert!(within_ttl(1_000, 1_000));
        assert!(within_ttl(1_000, 1_000 + REVERIFY_TTL_MS));
        assert!(!within_ttl(1_000, 1_000 + REVERIFY_TTL_MS + 1));
        // A backwards clock must neither panic nor falsely expire the entry.
        assert!(within_ttl(5_000, 1_000));
    }

    #[test]
    fn eviction_bounds_distinct_files() {
        let cache = FileSizeCache::new();
        for index in 0..(MAX_CACHED_FILE_SIZES + 10) {
            cache.insert(
                PathBuf::from(format!("/a/file{index}.ts")),
                1,
                computation(index as u64),
            );
        }
        assert!(cache.len() <= MAX_CACHED_FILE_SIZES);
    }

    fn unresolvable_context() -> AnalysisContext {
        AnalysisContext {
            workspace_root: PathBuf::from("/does/not/exist"),
            active_document_path: PathBuf::from("/does/not/exist/src/index.ts"),
        }
    }

    fn named_request(package: &str, named: &[&str]) -> ImportRequest {
        ImportRequest {
            specifier: package.to_owned(),
            package_name: package.to_owned(),
            version: "1.0.0".to_owned(),
            named: named.iter().map(|name| (*name).to_owned()).collect(),
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }
    }

    #[test]
    fn file_size_signature_changes_when_entry_bytes_change() {
        let _generation = hold_generation_steady();
        // Real node_modules fixture so resolve_package_entry succeeds and the
        // Ok(resolved) arm folds in the entry+manifest fingerprint.
        let ws = std::env::temp_dir().join(format!("il-l1-sig-{}", std::process::id()));
        let pkg = ws.join("node_modules").join("l1-lib");
        std::fs::create_dir_all(&pkg).expect("pkg");
        std::fs::write(
            pkg.join("package.json"),
            r#"{"version":"1.0.0","module":"index.js"}"#,
        )
        .expect("manifest");
        std::fs::write(pkg.join("index.js"), "export const a = 1;").expect("entry v1");

        let context = AnalysisContext {
            workspace_root: ws.clone(),
            active_document_path: ws.join("src").join("index.ts"),
        };
        let imports = vec![sized(ImportRequest {
            specifier: "l1-lib".to_owned(),
            package_name: "l1-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Namespace,
            runtime: ImportRuntime::Component,
        })];

        let sig1 = file_size_signature(&context, &imports);
        // Change the entry's content+length; the signature must change even though
        // the cache key no longer carries entry fingerprints.
        std::fs::write(pkg.join("index.js"), "export const a = 222222;").expect("entry v2");
        let sig2 = file_size_signature(&context, &imports);

        assert_ne!(
            sig1, sig2,
            "L1 signature must react to a resolved entry's content change"
        );
        std::fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn signature_is_order_independent() {
        let _generation = hold_generation_steady();
        let ctx = unresolvable_context();
        let a = sized(named_request("alpha", &["x"]));
        let b = sized(named_request("beta", &["y"]));
        assert_eq!(
            file_size_signature(&ctx, &[a.clone(), b.clone()]),
            file_size_signature(&ctx, &[b, a])
        );
    }

    #[test]
    fn signature_changes_when_named_exports_change() {
        let _generation = hold_generation_steady();
        let ctx = unresolvable_context();
        assert_ne!(
            file_size_signature(&ctx, &[sized(named_request("alpha", &["x"]))]),
            file_size_signature(&ctx, &[sized(named_request("alpha", &["x", "y"]))])
        );
    }

    /// A not-installed import is part of the file's identity: it is what makes the total a floor
    /// (FR-024a), and a signature that cannot see it would serve the L1 entry computed before the
    /// import was added. Fails if the import is dropped from the token list.
    #[test]
    fn signature_sees_an_import_whose_package_is_not_installed() {
        let _generation = hold_generation_steady();
        let ctx = unresolvable_context();
        let installed = sized(named_request("alpha", &["x"]));

        assert_ne!(
            file_size_signature(&ctx, std::slice::from_ref(&installed)),
            file_size_signature(&ctx, &[installed, SizedImport::not_installed("ghost")]),
        );
    }

    #[test]
    fn signature_changes_when_generation_bumps() {
        let _generation = hold_generation_steady();
        let ctx = unresolvable_context();
        let imports = [sized(named_request("alpha", &["x"]))];
        let before = file_size_signature(&ctx, &imports);
        bump_cache_generation();
        assert_ne!(before, file_size_signature(&ctx, &imports));
    }

    #[test]
    fn resolved_import_token_folds_first_party_deep_module_stat() {
        use crate::engine::dependency_paths::{clear, record_loaded_paths};
        use crate::pipeline::resolver::SideEffectsMode;

        // A first-party fixture lives OUTSIDE node_modules: entry imports a deep
        // module, both editable. Finding 8: editing the deep module must move L1.
        let root = std::env::temp_dir().join(format!("il-l1-fp-{}", std::process::id()));
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).expect("pkg dir");
        std::fs::write(
            pkg.join("package.json"),
            r#"{"version":"1.0.0","module":"index.js"}"#,
        )
        .expect("manifest");
        std::fs::write(
            pkg.join("index.js"),
            "import './deep.js';\nexport const a = 1;\n",
        )
        .expect("entry");
        std::fs::write(pkg.join("deep.js"), "export const d = 1;\n").expect("deep v1");

        let entry_path = std::fs::canonicalize(pkg.join("index.js")).expect("canonical entry");
        assert!(
            !path_has_node_modules_segment(&entry_path),
            "test setup: fixture must be first-party (no node_modules segment)"
        );

        let resolved = ResolvedPackage {
            package_root: pkg.clone(),
            package_json: serde_json::Value::Null,
            entry_path: entry_path.clone(),
            is_cjs: false,
            side_effects: SideEffectsMode::Unknown,
        };
        let request = ImportRequest {
            specifier: "pkg".to_owned(),
            package_name: "pkg".to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Namespace,
            runtime: ImportRuntime::Component,
        };

        clear();
        record_loaded_paths(
            entry_path.clone(),
            ImportRuntime::Component,
            vec![entry_path.clone(), pkg.join("deep.js")],
        );
        let token_before = resolved_import_token(&request, &resolved);

        // Edit the DEEP module only (not the entry, not the manifest): its len+mtime move.
        std::fs::write(pkg.join("deep.js"), "export const d = 22222222;\n").expect("deep v2");
        let token_after = resolved_import_token(&request, &resolved);

        assert_ne!(
            token_before, token_after,
            "editing a deep first-party module must change the L1 token"
        );
        clear();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolved_import_token_ignores_node_modules_internal_modules() {
        use crate::pipeline::resolver::SideEffectsMode;

        // Cost bound: a node_modules import must keep the cheap entry+manifest stat and
        // NOT re-stat its internal modules (they change only via install → generation).
        let root = std::env::temp_dir().join(format!("il-l1-nm-{}", std::process::id()));
        let pkg = root.join("node_modules").join("lib");
        std::fs::create_dir_all(&pkg).expect("pkg dir");
        std::fs::write(
            pkg.join("package.json"),
            r#"{"version":"1.0.0","module":"index.js"}"#,
        )
        .expect("manifest");
        std::fs::write(
            pkg.join("index.js"),
            "import './deep.js';\nexport const a = 1;\n",
        )
        .expect("entry");
        std::fs::write(pkg.join("deep.js"), "export const d = 1;\n").expect("deep v1");

        let entry_path = std::fs::canonicalize(pkg.join("index.js")).expect("canonical entry");
        assert!(
            path_has_node_modules_segment(&entry_path),
            "test setup: fixture must be under node_modules"
        );

        let resolved = ResolvedPackage {
            package_root: pkg.clone(),
            package_json: serde_json::Value::Null,
            entry_path: entry_path.clone(),
            is_cjs: false,
            side_effects: SideEffectsMode::Unknown,
        };
        let request = ImportRequest {
            specifier: "lib".to_owned(),
            package_name: "lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: ImportKind::Namespace,
            runtime: ImportRuntime::Component,
        };

        let token_before = resolved_import_token(&request, &resolved);
        // Editing an internal module must leave the token unchanged.
        std::fs::write(pkg.join("deep.js"), "export const d = 22222222;\n").expect("deep v2");
        let token_after = resolved_import_token(&request, &resolved);
        assert_eq!(
            token_before, token_after,
            "a node_modules import must not fold its internal modules into L1"
        );

        // Sanity: the entry itself is still covered, so a reinstall that rewrites the
        // entry still moves L1.
        std::fs::write(pkg.join("index.js"), "export const a = 22222222;\n").expect("entry v2");
        let token_entry_changed = resolved_import_token(&request, &resolved);
        assert_ne!(
            token_before, token_entry_changed,
            "editing the node_modules entry must change the L1 token"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
