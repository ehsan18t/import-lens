use crate::{
    cache::{key::cache_key_for_resolved_import, memory::cache_generation},
    ipc::protocol::ImportRequest,
    pipeline::{
        analyze::AnalysisContext, file_size::FileSizeComputation, resolver::resolve_package_entry,
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
// Distinct files are bounded by LRU eviction, mirroring GRAPH_CACHE.
const MAX_CACHED_FILE_SIZES: usize = 64;

// Bound how long an aggregate is served without any freshness signal. The
// signature only fingerprints each package's entry + manifest (via the cache
// key) plus the generation, NOT the transitive graph that `compute_file_size`
// bundles. A node_modules content change with no watcher event (e.g. a
// watcher-excluded folder) is reflected in the L2 per-import cache after its own
// re-verify window but would otherwise never reach L1. This TTL gives L1 the
// same backstop as `memory::REVERIFY_TTL_MS`, capping staleness to one window.
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
        entry.last_used_millis.store(now, Ordering::Relaxed);
        Some(entry.computation.clone())
    }

    pub fn insert(&self, path: PathBuf, signature: u64, computation: FileSizeComputation) {
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
        // entry, mirroring GRAPH_CACHE. Editing the same file never triggers
        // this because it overwrites one slot rather than adding a new key.
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
            .filter(|(path, _)| !path.exists())
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

/// Freshness key for an L1 file-size entry: sorted resolved per-import cache keys
/// (which fold in each package's manifest + entry fingerprint) plus the cache
/// generation. Unresolvable requests contribute a stable request-shape token.
/// Sorting makes it order-independent, matching `compute_file_size` which
/// combines all imports regardless of order.
pub fn file_size_signature(context: &AnalysisContext, requests: &[ImportRequest]) -> u64 {
    let mut tokens = requests
        .iter()
        .map(
            |request| match resolve_package_entry(&context.active_document_path, request) {
                Ok(resolved) => cache_key_for_resolved_import(request, &resolved),
                Err(_) => format!(
                    "unresolved:{}:{}:{}:{:?}:{:?}:{}",
                    request.package_name,
                    request.specifier,
                    request.version,
                    request.runtime,
                    request.import_kind,
                    request.named.join(",")
                ),
            },
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::memory::bump_cache_generation;
    use crate::ipc::protocol::{ImportKind, ImportRuntime};

    fn computation(minified: u64) -> FileSizeComputation {
        FileSizeComputation {
            minified_bytes: minified,
            ..FileSizeComputation::default()
        }
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
    fn signature_is_order_independent() {
        let ctx = unresolvable_context();
        let a = named_request("alpha", &["x"]);
        let b = named_request("beta", &["y"]);
        assert_eq!(
            file_size_signature(&ctx, &[a.clone(), b.clone()]),
            file_size_signature(&ctx, &[b, a])
        );
    }

    #[test]
    fn signature_changes_when_named_exports_change() {
        let ctx = unresolvable_context();
        assert_ne!(
            file_size_signature(&ctx, &[named_request("alpha", &["x"])]),
            file_size_signature(&ctx, &[named_request("alpha", &["x", "y"])])
        );
    }

    #[test]
    fn signature_changes_when_generation_bumps() {
        let ctx = unresolvable_context();
        let reqs = [named_request("alpha", &["x"])];
        let before = file_size_signature(&ctx, &reqs);
        bump_cache_generation();
        assert_ne!(before, file_size_signature(&ctx, &reqs));
    }
}
