use crate::{
    cache::{
        disk::DiskCache,
        key::{
            FileFingerprint, cache_key_is_orphan, cache_key_matches_any_package,
            fingerprints_are_current,
        },
    },
    ipc::protocol::ImportResult,
};
use papaya::HashMap;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const RECENT_PRELOAD_LIMIT: usize = 20;

// Cap the in-memory entry count so a long multi-package session cannot grow the
// map without bound. Eviction drops the least-recently-used entry; its disk copy
// (if any) survives and re-hydrates on the next hit. The cap is generous, so it
// only triggers in very large sessions.
pub const MAX_MEMORY_ENTRIES: usize = 4096;

// Dependency fingerprints only change when node_modules changes, which the
// extension signals via cache invalidation. Between invalidations, re-stat'ing
// every dependency file on each cache hit is pure waste, so a cached entry
// verified at the current generation skips the re-stat. The TTL backstops the
// case where node_modules changes with no invalidation event (e.g. a
// watcher-excluded folder): after it elapses, the entry re-verifies anyway.
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(1);
const REVERIFY_TTL_MS: u64 = 30_000;

pub fn bump_cache_generation() {
    CACHE_GENERATION.fetch_add(1, Ordering::Release);
}

fn current_cache_generation() -> u64 {
    CACHE_GENERATION.load(Ordering::Acquire)
}

/// Public reader for the global cache generation. The file-size L1 cache folds
/// this into its freshness signature so a node_modules invalidation forces every
/// file entry to recompute.
pub fn cache_generation() -> u64 {
    current_cache_generation()
}

#[derive(Debug, Clone)]
pub struct CachedImport {
    pub result: ImportResult,
    pub dependency_fingerprints: Vec<FileFingerprint>,
    // Runtime verification state (not persisted): the generation and wall-clock
    // at which this entry's fingerprints were last confirmed current.
    pub verified_generation: u64,
    pub verified_at_millis: u64,
    // Wall-clock of the last cache hit; drives LRU eviction. Shared via Arc so a
    // hit can bump it in place without re-inserting the entry.
    pub last_used_millis: Arc<AtomicU64>,
}

#[derive(Debug)]
pub struct ImportCache {
    memory: HashMap<String, CachedImport>,
    disk: DiskCache,
    // Keys whose synchronous disk insert failed; flush_to_disk replays these.
    dirty: Mutex<HashSet<String>>,
}

impl Default for ImportCache {
    fn default() -> Self {
        Self {
            memory: HashMap::new(),
            disk: DiskCache::default(),
            dirty: Mutex::new(HashSet::new()),
        }
    }
}

impl ImportCache {
    pub fn new(storage_path: Option<PathBuf>, enable_disk_cache: bool) -> Self {
        Self::new_with_recent_preload_limit(storage_path, enable_disk_cache, RECENT_PRELOAD_LIMIT)
    }

    pub fn new_with_recent_preload_limit(
        storage_path: Option<PathBuf>,
        enable_disk_cache: bool,
        recent_preload_limit: usize,
    ) -> Self {
        let memory = HashMap::new();
        let disk = DiskCache::new(storage_path, enable_disk_cache);

        {
            let pinned = memory.pin();
            for (key, cached) in disk.load_recent(recent_preload_limit) {
                pinned.insert(key, cached);
            }
        }

        Self {
            memory,
            disk,
            dirty: Mutex::new(HashSet::new()),
        }
    }

    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let memory = self.memory.pin();
        if let Some(cached) = memory.get(key) {
            let generation = current_cache_generation();
            let now = crate::time::unix_millis_now();
            // Bump LRU recency on every hit. The Arc is shared with the restamp
            // clone below, so this stays current across both return paths.
            cached.last_used_millis.store(now, Ordering::Relaxed);
            let fresh_without_restat = cached.verified_generation == generation
                && now.saturating_sub(cached.verified_at_millis) <= REVERIFY_TTL_MS;

            if !fresh_without_restat {
                if !fingerprints_are_current(&cached.dependency_fingerprints) {
                    memory.remove(key);
                    self.disk.remove(key);
                    return None;
                }
                // Re-verified: restamp so subsequent hits can skip the re-stat.
                // Clone the result once from the restamped entry rather than
                // re-fetching the entry and cloning again after the insert. The
                // stored entry keeps cache_hit=false; only the returned copy is
                // flagged as a hit.
                let mut restamped = cached.clone();
                restamped.verified_generation = generation;
                restamped.verified_at_millis = now;
                let mut result = restamped.result.clone();
                memory.insert(key.to_owned(), restamped);
                result.cache_hit = true;
                self.disk.touch(key);
                return Some(result);
            }

            let mut result = cached.result.clone();
            result.cache_hit = true;
            self.disk.touch(key);
            return Some(result);
        }

        if let Some(mut cached) = self.disk.get(key) {
            // DiskCache::get already verified fingerprints against the file, so
            // stamp the entry current to avoid a redundant re-stat on next hit.
            cached.verified_generation = current_cache_generation();
            cached.verified_at_millis = crate::time::unix_millis_now();
            let mut result = cached.result.clone();
            memory.insert(key.to_owned(), cached);
            result.cache_hit = true;
            drop(memory);
            // Re-hydrating from disk grows the map too, so enforce the cap here as
            // well as on fresh inserts.
            self.enforce_memory_cap();
            return Some(result);
        }

        None
    }

    pub fn insert(&self, key: String, result: ImportResult) {
        self.insert_with_fingerprints(key, result, Vec::new());
    }

    pub fn insert_with_fingerprints(
        &self,
        key: String,
        result: ImportResult,
        dependency_fingerprints: Vec<FileFingerprint>,
    ) {
        let now = crate::time::unix_millis_now();
        let cached = CachedImport {
            result,
            dependency_fingerprints,
            verified_generation: current_cache_generation(),
            verified_at_millis: now,
            last_used_millis: Arc::new(AtomicU64::new(now)),
        };

        if let Err(error) = self.disk.insert(&key, &cached) {
            crate::logging::log_warn("cache", format!("skipping disk insert for {key}: {error}"));
            if let Ok(mut dirty) = self.dirty.lock() {
                dirty.insert(key.clone());
            }
        }

        self.memory.pin().insert(key, cached);
        self.enforce_memory_cap();
    }

    /// Evicts the least-recently-used entry while the in-memory map is over the
    /// cap. The disk copy (if any) survives and re-hydrates on the next hit, so
    /// this only sheds the memory mirror. Called from every path that grows the
    /// map (fresh insert and disk re-hydration), not the restamp path (which
    /// replaces an existing key and cannot grow the map).
    fn enforce_memory_cap(&self) {
        let memory = self.memory.pin();
        while memory.len() > MAX_MEMORY_ENTRIES {
            let Some(oldest) = memory
                .iter()
                .min_by_key(|(_, entry)| entry.last_used_millis.load(Ordering::Relaxed))
                .map(|(oldest_key, _)| oldest_key.clone())
            else {
                break;
            };
            memory.remove(&oldest);
        }
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.invalidate_packages(&HashSet::from([package_name.to_owned()]));
    }

    /// Evicts every entry for any package in `package_names` from both the disk
    /// and memory layers in a single scan per layer (each key decoded once),
    /// rather than one full scan per package.
    pub fn invalidate_packages(&self, package_names: &HashSet<String>) {
        if package_names.is_empty() {
            return;
        }
        self.disk.invalidate_packages(package_names);

        let memory = self.memory.pin();
        let keys = memory
            .iter()
            .filter(|(key, _)| cache_key_matches_any_package(key, package_names))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        for key in keys {
            memory.remove(&key);
        }
    }

    /// Drops orphaned entries (release-stale analyzer version, or a resolved
    /// package/entry path that no longer exists) from both layers. Returns the
    /// number removed from disk.
    pub fn purge_orphan_entries(&self, current_analyzer_version: &str) -> usize {
        let removed = self.disk.purge_orphan_entries(current_analyzer_version);

        let memory = self.memory.pin();
        let keys = memory
            .iter()
            .filter(|(key, _)| cache_key_is_orphan(key, current_analyzer_version))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            memory.remove(&key);
        }

        removed
    }

    pub fn clear(&self) {
        self.disk.clear();
        self.memory.pin().clear();
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.clear();
        }
    }

    pub fn memory_len(&self) -> usize {
        self.memory.pin().len()
    }

    pub fn recent_keys(&self, limit: usize) -> Vec<String> {
        self.disk.recent_keys(limit)
    }

    pub fn pending_recency_touch_count(&self) -> usize {
        self.disk.pending_touch_len()
    }

    pub fn flush_recency_touches(&self) {
        self.disk.flush_pending_touches();
    }

    // Inserts are queued in the disk cache for batched commit; a recycle must
    // drain that queue. Any entry whose enqueue failed (serialization error) is
    // marked dirty and re-enqueued here before the queue is flushed.
    pub fn flush_to_disk(&self) -> Result<(), String> {
        let dirty_keys = match self.dirty.lock() {
            Ok(mut dirty) => std::mem::take(&mut *dirty),
            Err(_) => return Err("cache dirty-set lock poisoned".to_owned()),
        };

        let entries = {
            let memory = self.memory.pin();
            dirty_keys
                .iter()
                .filter_map(|key| memory.get(key).map(|cached| (key.clone(), cached.clone())))
                .collect::<Vec<_>>()
        };

        for (key, cached) in entries {
            if let Err(error) = self.disk.insert(&key, &cached) {
                if let Ok(mut dirty) = self.dirty.lock() {
                    dirty.extend(dirty_keys);
                }
                return Err(error);
            }
        }

        self.disk.flush_pending_inserts();
        self.disk.flush_pending_touches();
        Ok(())
    }
}
