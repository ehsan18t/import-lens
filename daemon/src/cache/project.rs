use crate::{
    cache::budget::{BudgetCoordinator, EvictableShard, EvictionOutcome, MaintenanceOutcome},
    cache::disk::ShardRollup,
    cache::memory::ImportCache,
    ipc::protocol::{CacheOperationResult, CacheShardInfo},
    time::unix_millis_now,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const SHARD_METADATA_FILE_NAME: &str = "importlens-project-cache.json";
const SHARD_DB_FILE_NAME: &str = "importlens.redb";
const LEGACY_CENTRAL_CACHE_DB_FILE_NAME: &str = "importlens.redb";
const LEGACY_CENTRAL_CACHE_SHARD_ID: &str = "legacy-central";
const PROJECT_METADATA_WRITE_INTERVAL_MILLIS: u64 = 60_000;
const AGGREGATE_OVER_BUDGET_COMPACT_THRESHOLD: f64 = 0.0;
/// Minimum wall-clock gap between automatic orphan-shard sweeps on the
/// maintenance tick (RB-17). Abandoned-project detection is rare and the sweep
/// stats every shard root, so it runs far less often than the 60 s tick.
const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct ProjectCacheRegistry {
    base_path: Option<PathBuf>,
    enable_disk_cache: bool,
    max_size_mb: u64,
    loaded: Mutex<HashMap<String, LoadedProjectCache>>,
    // Per-shard "load lock" (Finding 11): serializes concurrent COLD opens of the
    // SAME shard so its database is opened exactly once, WITHOUT holding `loaded`
    // across the `Database::create` + `load_recent` + metadata `fs::write`. A
    // shard's `Arc<Mutex<()>>` is only ever locked while `loaded` is NOT held;
    // `loaded` is then re-acquired briefly (double-check + register). Lock order is
    // always load-lock -> loaded (held briefly, released), never the reverse — no
    // cycle. This map's own mutex is a leaf, held only for the get-or-insert of a
    // shard's lock Arc.
    load_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    // Owns the global disk-byte budget and cross-shard LRU eviction. The budget
    // derives from `max_size_mb` in production (`new`) and is injected directly
    // by tests (`new_with_budget_bytes`); 0 disables it.
    coordinator: BudgetCoordinator,
    // Last time the automatic orphan-shard sweep ran (RB-17). Throttles the sweep
    // to `ORPHAN_SWEEP_INTERVAL` so the rare-need scan doesn't stat every shard
    // root on every 60 s maintenance tick.
    last_orphan_sweep: Mutex<Option<Instant>>,
}

/// A loaded-or-temporarily-opened shard the budget evictor can inspect and trim.
struct ShardTarget {
    shard_id: String,
    cache: Arc<ImportCache>,
}

impl EvictableShard for ShardTarget {
    fn shard_id(&self) -> &str {
        &self.shard_id
    }

    fn rollup(&self) -> ShardRollup {
        self.cache.shard_rollup()
    }

    fn lowest_seq_keys(&self, n: usize, floor: u64) -> Vec<String> {
        self.cache.lowest_seq_disk_keys(n, floor)
    }

    fn evict_keys(&self, keys: &[String]) -> u64 {
        self.cache.evict_keys(keys)
    }
}

trait CompactableTarget {
    fn compact_if_fragmented(&self, threshold: f64) -> bool;
}

impl CompactableTarget for ShardTarget {
    fn compact_if_fragmented(&self, threshold: f64) -> bool {
        self.cache.compact_if_fragmented(threshold)
    }
}

fn compact_targets<T: CompactableTarget + ?Sized>(targets: &[&T], threshold: f64) -> usize {
    targets
        .iter()
        .filter(|target| target.compact_if_fragmented(threshold))
        .count()
}

fn aggressive_compact_targets_if_physical_over_budget<T: CompactableTarget + ?Sized>(
    targets: &[&T],
    physical_bytes: u64,
    budget_bytes: u64,
) -> usize {
    if physical_bytes <= budget_bytes {
        return 0;
    }
    compact_targets(targets, AGGREGATE_OVER_BUDGET_COMPACT_THRESHOLD)
}

#[derive(Debug, Clone)]
struct LoadedProjectCache {
    project_root: String,
    normalized_root: String,
    cache_path: PathBuf,
    cache: Arc<ImportCache>,
    last_used_millis: u64,
    last_metadata_write_millis: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectCacheMetadata {
    shard_id: String,
    project_root: String,
    normalized_root: String,
    last_used_millis: u64,
}

impl ProjectCacheRegistry {
    pub fn new(base_path: Option<PathBuf>, enable_disk_cache: bool, max_size_mb: u64) -> Self {
        let budget_bytes = max_size_mb.saturating_mul(1024 * 1024);
        Self::new_with_budget_bytes(base_path, enable_disk_cache, max_size_mb, budget_bytes)
    }

    /// Like `new`, but with an explicit byte budget instead of deriving it from
    /// `max_size_mb`. Lets tests exercise the evictor with a small budget without
    /// inserting megabytes of entries.
    pub fn new_with_budget_bytes(
        base_path: Option<PathBuf>,
        enable_disk_cache: bool,
        max_size_mb: u64,
        budget_bytes: u64,
    ) -> Self {
        Self {
            base_path,
            enable_disk_cache,
            max_size_mb,
            loaded: Mutex::new(HashMap::new()),
            load_locks: Mutex::new(HashMap::new()),
            coordinator: BudgetCoordinator::new(budget_bytes),
            last_orphan_sweep: Mutex::new(None),
        }
    }

    /// Startup recency seed (C5 / Finding 10d, §3.3): before the server accepts any
    /// request, lift the process-global recency clock above the GLOBAL maximum
    /// persisted seq across every on-disk shard. The clock resets to 1 each process
    /// start, so without this a fresh post-restart access (small seq) could sort as
    /// *older* than an untouched prior-session shard's durable entries — inverting
    /// the evictor's smallest-`oldest_seq` victim selection and letting the active
    /// project be evicted before that stale shard. C1's per-shard `max_seq`
    /// high-water is already observed on rollup/hydration, but only for shards
    /// touched THIS session; this pass observes every shard's `max_seq` exactly once
    /// so no shard — and no entry created before the first maintenance rollup — is
    /// left unprotected.
    ///
    /// A no-op when the disk cache is disabled (no shards, nothing persisted). Reuses
    /// the maintenance shard enumeration: each unloaded shard is temp-opened with a
    /// zero recent-preload (no hydration scan) and its `max_seq` read via a single
    /// SUMMARY key — NOT a full CACHE_TABLE scan. One small read per project shard,
    /// the design's §3.3 "rebuild rollups from stored summary" pass, an acceptable
    /// one-time startup cost. Loaded shards (empty at true startup, but handled for
    /// robustness if ever called later) are observed from their live handles.
    pub fn seed_recency_clock_from_disk(&self) {
        if !self.storage_enabled() {
            crate::logging::log_debug("cache", "skipped recency seed; disk cache is disabled");
            return;
        }

        let started_at = Instant::now();
        let (loaded_count, loaded_ids) = self
            .loaded
            .lock()
            .map(|loaded| {
                for shard in loaded.values() {
                    crate::cache::recency::RecencyClock::observe(shard.cache.summary_max_seq());
                }
                (loaded.len(), loaded.keys().cloned().collect::<HashSet<_>>())
            })
            .unwrap_or_default();

        let mut scanned_shards = 0usize;
        for (shard_id, cache_path) in self.scan_disk_shard_paths() {
            if loaded_ids.contains(&shard_id) || cache_path.as_os_str().is_empty() {
                continue;
            }
            scanned_shards += 1;
            let cache = ImportCache::new_with_recent_preload_limit(
                Some(cache_path),
                self.enable_disk_cache,
                0,
            );
            crate::cache::recency::RecencyClock::observe(cache.summary_max_seq());
        }

        crate::logging::log_debug(
            "cache",
            format!(
                "seeded recency clock from disk in {}ms (loaded_shards={}, scanned_shards={})",
                started_at.elapsed().as_millis(),
                loaded_count,
                scanned_shards
            ),
        );
    }

    /// Enforces the global disk-byte budget by evicting the least-recently-used
    /// entries across all shards (loaded and on-disk) down to the low-water mark.
    /// Off the request hot path — driven by the periodic maintenance tick and on
    /// demand. A no-op when the disk cache is disabled or the budget is 0.
    pub fn evict_to_budget(&self) -> EvictionOutcome {
        if !self.enable_disk_cache || self.coordinator.budget_bytes() == 0 {
            return EvictionOutcome::default();
        }
        let targets = self.collect_shard_targets();
        let refs = targets
            .iter()
            .map(|target| target as &dyn EvictableShard)
            .collect::<Vec<_>>();
        self.coordinator.evict_to_budget(&refs)
    }

    /// One full maintenance pass: byte-budget eviction, then normal per-shard
    /// fragmentation compaction. If aggregate physical bytes still exceed the
    /// budget afterward, an aggressive idle compaction pass runs with a zero
    /// threshold so thinly-spread free pages can be reclaimed too. Both operate on
    /// the same target set — loaded shards plus temp-opened unloaded ones — so a
    /// drained unloaded shard's file shrinks too, keeping the PHYSICAL footprint
    /// tracking the budget, not just the logical one.
    ///
    /// Unless `force` is set (manual "clean up now"), a cheap physical-size gate
    /// runs first: every stored value lives inside its shard's `.redb` file, so
    /// the summed file sizes bound the logical total from above — at/below budget
    /// the full pass (which opens every unloaded shard and reads every seq
    /// prefix) is provably unnecessary and skipped. Queued inserts not yet
    /// flushed are invisible to the gate; the tick after their flush sees them.
    pub fn run_maintenance(&self, force: bool) -> MaintenanceOutcome {
        if !self.enable_disk_cache || self.coordinator.budget_bytes() == 0 {
            return MaintenanceOutcome::default();
        }
        if !force && self.total_shard_file_bytes() <= self.coordinator.budget_bytes() {
            return MaintenanceOutcome {
                skipped_under_budget: true,
                ..MaintenanceOutcome::default()
            };
        }

        let targets = self.collect_shard_targets();
        let refs = targets
            .iter()
            .map(|target| target as &dyn EvictableShard)
            .collect::<Vec<_>>();
        let eviction = self.coordinator.evict_to_budget(&refs);
        if eviction.still_over_budget {
            crate::logging::log_warn(
                "cache",
                format!(
                    "cache remains over the {} MB budget after eviction (all remaining \
                     entries are floor-protected or a shard is not accepting evictions)",
                    self.coordinator.budget_bytes() / (1024 * 1024)
                ),
            );
        }

        let compactable_targets = targets.iter().collect::<Vec<_>>();
        let mut compacted_shards =
            compact_targets(&compactable_targets, crate::cache::disk::COMPACT_THRESHOLD);
        compacted_shards += aggressive_compact_targets_if_physical_over_budget(
            &compactable_targets,
            self.total_shard_file_bytes(),
            self.coordinator.budget_bytes(),
        );

        MaintenanceOutcome {
            eviction,
            compacted_shards,
            skipped_under_budget: false,
        }
    }

    /// Assembles the eviction/compaction target set: loaded shards (shared Arcs)
    /// plus every on-disk shard not currently loaded, each temp-opened for the
    /// pass. The loaded snapshot is taken under the lock and released before any
    /// disk I/O. A temp open racing a concurrent `cache_for_root` degrades
    /// harmlessly on either side: the temp cache scans/evicts nothing, and the
    /// loading side serves one unregistered memory-only cache and heals on the
    /// next call.
    fn collect_shard_targets(&self) -> Vec<ShardTarget> {
        let (loaded_ids, mut targets) = match self.loaded.lock() {
            Ok(loaded) => {
                let ids = loaded.keys().cloned().collect::<HashSet<_>>();
                let targets = loaded
                    .iter()
                    .map(|(shard_id, shard)| ShardTarget {
                        shard_id: shard_id.clone(),
                        cache: Arc::clone(&shard.cache),
                    })
                    .collect::<Vec<_>>();
                (ids, targets)
            }
            Err(_) => (HashSet::new(), Vec::new()),
        };

        for (shard_id, cache_path) in self.scan_disk_shard_paths() {
            if loaded_ids.contains(&shard_id) || cache_path.as_os_str().is_empty() {
                continue;
            }
            let cache = Arc::new(ImportCache::new_with_recent_preload_limit(
                Some(cache_path),
                self.enable_disk_cache,
                0,
            ));
            targets.push(ShardTarget { shard_id, cache });
        }
        targets
    }

    /// Sum of every shard's `.redb` file size — a cheap upper bound on the
    /// logical cache total (values live inside the files), used to gate the full
    /// maintenance pass.
    fn total_shard_file_bytes(&self) -> u64 {
        let Some(base_path) = self.base_path.as_ref() else {
            return 0;
        };
        let Ok(entries) = fs::read_dir(base_path) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| fs::metadata(entry.path().join(SHARD_DB_FILE_NAME)).ok())
            .map(|metadata| metadata.len())
            .sum()
    }

    pub fn cache_for_root(&self, project_root: &Path) -> Arc<ImportCache> {
        let shard_id = project_cache_shard_id(project_root);
        let now = unix_millis_now();

        // Fast warm path: hold `loaded` only for the map lookup + timestamp bump,
        // then release it before any metadata `fs::write` so a warm hit never
        // blocks peers on disk I/O.
        if let Some(cache) = self.warm_shard_hit(&shard_id, now) {
            return cache;
        }

        // Cold path (Finding 11): open + register the shard WITHOUT holding
        // `loaded` across the `Database::create` + `load_recent` + metadata write.
        // A per-shard load lock serializes concurrent opens of THIS shard (so redb
        // sees a single open — no `DatabaseAlreadyOpen` self-race) while unrelated
        // shards load in parallel and `loaded` is held only for brief map ops.
        //
        // Lock order (no cycle): the per-shard load lock is acquired ONLY here,
        // after the warm path released `loaded`; `loaded` is then re-acquired
        // briefly inside it (the double-check and the register). `loaded` is never
        // held while taking the load lock.
        let load_lock = self.load_lock_for(&shard_id);
        let _load_guard = load_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Double-check: another thread may have finished loading this shard while
        // we waited on the per-shard load lock.
        if let Some(cache) = self.warm_shard_hit(&shard_id, now) {
            return cache;
        }

        // Sole opener for this shard now (we hold its load lock): perform the
        // expensive I/O with no global lock held.
        let normalized_root = normalize_project_root(project_root);
        let cache_path = self.cache_path_for_shard(&shard_id);
        let disk_path = self.disk_cache_path(&cache_path);
        let cache = Arc::new(ImportCache::new(disk_path, self.enable_disk_cache));
        // If persistence was requested but the open failed — most likely
        // `DatabaseAlreadyOpen` because a maintenance pass (eviction, invalidation,
        // orphan purge) temporarily holds this shard's file — do NOT register the
        // degraded cache: a registered shard is permanent (only user-triggered
        // removal evicts it from `loaded`), which would silently disable this
        // project's persistence until recycle. Serve a memory-only cache for this
        // call and let the next call retry the open.
        if self.storage_enabled() && !cache.disk_available() {
            return cache;
        }
        let shard = LoadedProjectCache {
            project_root: project_root.to_string_lossy().to_string(),
            normalized_root,
            cache_path,
            cache: Arc::clone(&cache),
            last_used_millis: now,
            last_metadata_write_millis: now,
        };
        // Metadata write stays off the `loaded` lock (still under the load lock).
        self.write_metadata_for_loaded(&shard_id, &shard);
        // Briefly re-acquire `loaded` to register the freshly-opened shard. We hold
        // this shard's load lock, so no concurrent cold path could have inserted it
        // — a plain insert cannot clobber a different Arc. A poisoned lock leaves
        // the shard unregistered; the returned (disk-backed) cache still serves
        // this call and the next call heals.
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded.insert(shard_id, shard);
        }
        cache
    }

    /// The warm path of `cache_for_root`: if the shard is already loaded, bump its
    /// last-used timestamp under `loaded`, clone its `Arc`, RELEASE `loaded`, then
    /// perform any throttled metadata `fs::write` off-lock (so a warm hit never
    /// blocks peers on disk I/O). Returns `None` — with `loaded` released — when the
    /// shard is absent, so the caller can take the cold path without holding it.
    fn warm_shard_hit(&self, shard_id: &str, now: u64) -> Option<Arc<ImportCache>> {
        let mut loaded = self.loaded.lock().ok()?;
        let shard = loaded.get_mut(shard_id)?;
        shard.last_used_millis = now;
        let pending_metadata =
            if should_write_project_metadata(shard.last_metadata_write_millis, now) {
                shard.last_metadata_write_millis = now;
                self.metadata_write_for_loaded(shard_id, shard)
            } else {
                None
            };
        let cache = Arc::clone(&shard.cache);
        drop(loaded);
        // The timestamp is advanced under the lock above, so only one thread per
        // interval captures a pending write.
        if let Some((path, metadata)) = pending_metadata {
            let _ = write_metadata(&path, &metadata);
        }
        Some(cache)
    }

    /// Returns this shard's per-shard load lock, creating it on first use. Holds the
    /// `load_locks` map mutex only for the get-or-insert — a leaf lock, never held
    /// across I/O, across `loaded`, or across the returned lock. Recovers a poisoned
    /// map so a prior panic can't wedge all future loads.
    fn load_lock_for(&self, shard_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .load_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry(shard_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub fn list_shards(&self) -> Vec<CacheShardInfo> {
        self.list_shards_with_rollups(&self.shard_rollups_by_id())
    }

    /// Builds the shard list and stamps each shard's `entry_count` from the
    /// supplied C1 rollups (O(1) per shard, keyed by shard id). Split out so a
    /// status request can reuse the SAME rollup map for the top-level
    /// `total_bytes`, opening each unloaded shard's summary at most once.
    fn list_shards_with_rollups(
        &self,
        rollups: &HashMap<String, ShardRollup>,
    ) -> Vec<CacheShardInfo> {
        let mut shards = self.scan_disk_shards();

        if let Ok(loaded) = self.loaded.lock() {
            for (shard_id, shard) in loaded.iter() {
                let info = self.info_for_loaded(shard_id, shard);
                if let Some(existing) = shards
                    .iter_mut()
                    .find(|candidate| candidate.shard_id == *shard_id)
                {
                    *existing = info;
                } else {
                    shards.push(info);
                }
            }
        }

        for shard in shards.iter_mut() {
            if let Some(rollup) = rollups.get(&shard.shard_id) {
                shard.entry_count = rollup.entry_count;
            }
        }

        shards.sort_by(|left, right| {
            right
                .size_bytes
                .cmp(&left.size_bytes)
                .then_with(|| left.project_root.cmp(&right.project_root))
        });
        shards
    }

    /// The C1 per-shard rollups keyed by shard id: loaded shards read from their
    /// live handles, unloaded shards temp-opened exactly as the maintenance pass
    /// does ([`Self::collect_shard_targets`]). Each rollup is a few SUMMARY
    /// scalars — O(1) per shard, never a CACHE_TABLE scan — so status/list
    /// observability (§8/X-24) stays cheap. A temp open racing a concurrent load
    /// degrades harmlessly (one side reads an empty rollup / serves memory-only
    /// for a single call and self-heals).
    fn shard_rollups_by_id(&self) -> HashMap<String, ShardRollup> {
        self.collect_shard_targets()
            .into_iter()
            .map(|target| {
                let rollup = target.cache.shard_rollup();
                (target.shard_id, rollup)
            })
            .collect()
    }

    pub fn status_for_root(&self, project_root: Option<&Path>) -> ProjectCacheStatus {
        // One rollup pass feeds BOTH each shard's `entry_count` and the top-level
        // `total_bytes`, so a status request opens each unloaded shard's O(1)
        // summary at most once.
        let rollups = self.shard_rollups_by_id();
        let shards = self.list_shards_with_rollups(&rollups);
        let total_size_bytes = shards.iter().map(|shard| shard.size_bytes).sum();
        let total_bytes = rollups
            .values()
            .fold(0u64, |acc, rollup| acc.saturating_add(rollup.total_bytes));
        let normalized_root = project_root.map(normalize_project_root);
        let current_project = normalized_root.and_then(|root| {
            shards
                .iter()
                .find(|shard| shard.normalized_root == root)
                .cloned()
        });
        ProjectCacheStatus {
            total_size_bytes,
            total_bytes,
            budget_bytes: self.coordinator.budget_bytes(),
            project_count: shards.len(),
            max_size_mb: self.max_size_mb,
            current_project,
        }
    }

    pub fn remove_current_project(&self, project_root: &Path) -> Vec<CacheOperationResult> {
        vec![self.remove_shard_by_id(&project_cache_shard_id(project_root))]
    }

    pub fn remove_selected(&self, shard_ids: &[String]) -> Vec<CacheOperationResult> {
        shard_ids
            .iter()
            .map(|shard_id| self.remove_shard_by_id(shard_id))
            .collect()
    }

    pub fn remove_all(&self) -> Vec<CacheOperationResult> {
        let mut shard_ids = self
            .list_shards()
            .into_iter()
            .map(|shard| shard.shard_id)
            .collect::<Vec<_>>();
        shard_ids.sort();
        shard_ids.dedup();
        shard_ids
            .iter()
            .map(|shard_id| self.remove_shard_by_id(shard_id))
            .collect()
    }

    /// Whether a shard's project root is a genuine orphan (its volume is live but
    /// the folder is gone), safe to destroy. Drive-safe via `classify_project_root`
    /// (X-3 / RB-7): an unplugged/offline drive, or a shard with no recorded root,
    /// is never treated as orphaned.
    fn shard_root_is_orphaned(&self, shard: &CacheShardInfo) -> bool {
        !shard.project_root.is_empty()
            && crate::cache::key::classify_project_root(Path::new(&shard.project_root))
                == crate::cache::key::ProjectRootState::Orphaned
    }

    /// Manual orphan reclaim (Manage-Cache "Remove Orphaned Caches", RB-17).
    /// Removes shards whose project root is genuinely gone — drive-safe, so an
    /// unplugged/offline drive keeps its shard (X-3 / RB-7) — and drops
    /// stale/uninstalled entries from surviving shards. Stat-only (no project-tree
    /// walk). Returns the removed-shard results (entry-level drops are not surfaced
    /// per entry). The automatic maintenance-tick sweep is the shard-only
    /// `sweep_orphaned_shards_if_due`; this manual pass additionally scrubs entries.
    pub fn purge_orphans(&self) -> Vec<CacheOperationResult> {
        let analyzer_version = crate::cache::key::ANALYZER_VERSION;
        let loaded_ids = self
            .loaded
            .lock()
            .map(|loaded| loaded.keys().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();

        let mut removed = Vec::new();
        for shard in self.list_shards() {
            if self.shard_root_is_orphaned(&shard) {
                removed.push(self.remove_shard_by_id(&shard.shard_id));
                continue;
            }

            if loaded_ids.contains(&shard.shard_id) {
                // Clone the Arc out and release the lock before the scan+write, so
                // the purge doesn't block peers needing the loaded map.
                let cache = self.loaded.lock().ok().and_then(|loaded| {
                    loaded
                        .get(&shard.shard_id)
                        .map(|entry| Arc::clone(&entry.cache))
                });
                if let Some(cache) = cache {
                    cache.purge_orphan_entries(analyzer_version);
                }
            } else if !shard.cache_path.is_empty() {
                let cache = ImportCache::new_with_recent_preload_limit(
                    Some(PathBuf::from(&shard.cache_path)),
                    self.enable_disk_cache,
                    0,
                );
                cache.purge_orphan_entries(analyzer_version);
            }
        }

        removed
    }

    /// Automatic orphan-shard reclaim for the maintenance tick (RB-17). Removes
    /// ONLY shards whose project root is genuinely gone (drive-safe); entry-level
    /// staleness is already reclaimed automatically (name invalidation + the
    /// freshness `Gone` eviction), so this leaves surviving shards untouched.
    /// Throttled to `ORPHAN_SWEEP_INTERVAL` — a no-op (returns empty) until due —
    /// because an abandoned-project scan is rare and stats every shard root.
    /// Returns the removed-shard results.
    pub fn sweep_orphaned_shards_if_due(&self) -> Vec<CacheOperationResult> {
        {
            let mut last = self
                .last_orphan_sweep
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            if matches!(*last, Some(previous) if now.duration_since(previous) < ORPHAN_SWEEP_INTERVAL)
            {
                return Vec::new();
            }
            *last = Some(now);
        }

        self.list_shards()
            .iter()
            .filter(|shard| self.shard_root_is_orphaned(shard))
            .map(|shard| self.remove_shard_by_id(&shard.shard_id))
            .collect()
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.invalidate_packages(&[package_name.to_owned()]);
    }

    /// Invalidates every named package across all loaded and on-disk shards in a
    /// single pass: each on-disk shard's database is opened once (not once per
    /// package), and the recursive per-shard size walk is skipped since only ids
    /// and paths are needed for invalidation.
    pub fn invalidate_packages(&self, package_names: &[String]) {
        if package_names.is_empty() {
            return;
        }

        let package_set: HashSet<String> = package_names.iter().cloned().collect();

        // Snapshot the loaded shards' ids + cache Arcs under the lock, then RELEASE
        // it before the per-shard redb write scans (Finding 11): a single
        // `NodeModulesChanged` invalidation must not stall in-flight parallel
        // analysis for EVERY other project for the length of an N-shard rewrite.
        // redb serializes each shard's own writer, so the cloned-Arc writes need no
        // global lock. The id set fixes exactly which shards the disk loop below
        // must skip (unchanged from holding the lock across the writes).
        let (loaded_ids, loaded_caches) = self
            .loaded
            .lock()
            .map(|loaded| {
                let ids = loaded.keys().cloned().collect::<HashSet<_>>();
                let caches = loaded
                    .values()
                    .map(|shard| Arc::clone(&shard.cache))
                    .collect::<Vec<_>>();
                (ids, caches)
            })
            .unwrap_or_default();

        for cache in loaded_caches {
            cache.invalidate_packages(&package_set);
        }

        for (shard_id, cache_path) in self.scan_disk_shard_paths() {
            if loaded_ids.contains(&shard_id) || cache_path.as_os_str().is_empty() {
                continue;
            }
            let cache = ImportCache::new_with_recent_preload_limit(
                Some(cache_path),
                self.enable_disk_cache,
                0,
            );
            cache.invalidate_packages(&package_set);
        }
    }

    pub fn clear_all(&self) {
        let _ = self.remove_all();
    }

    pub fn memory_len(&self) -> usize {
        if let Ok(loaded) = self.loaded.lock() {
            return loaded.values().map(|shard| shard.cache.memory_len()).sum();
        }

        0
    }

    pub fn recent_keys(&self, project_root: &Path, limit: usize) -> Vec<String> {
        self.cache_for_root(project_root).recent_keys(limit)
    }

    pub fn flush_to_disk(&self) -> Result<(), String> {
        // Recover a poisoned lock (matching this file's convention, e.g. `cache_for_root`)
        // rather than propagating: RB-10's whole point is to flush every shard we can, so
        // a poisoned `loaded` (a panic under a brief map op) must not skip ALL flushes.
        let caches = {
            let loaded = self
                .loaded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loaded
                .iter()
                .map(|(shard_id, shard)| (shard_id.clone(), Arc::clone(&shard.cache)))
                .collect::<Vec<_>>()
        };

        let mut errors = Vec::new();
        for (shard_id, cache) in caches {
            if let Err(error) = cache.flush_to_disk() {
                errors.push(format!("{shard_id}: {error}"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn remove_shard_by_id(&self, shard_id: &str) -> CacheOperationResult {
        // Removal is the destructive sibling of the cold-open path: take the same
        // per-shard load lock so a cold opener cannot register a shard while its
        // directory is being deleted. Preserve the global ordering used by
        // `cache_for_root`: load-lock first, then `loaded` only for a brief map op.
        let load_lock = self.load_lock_for(shard_id);
        let _load_guard = load_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let loaded = self
            .loaded
            .lock()
            .ok()
            .and_then(|mut loaded| loaded.remove(shard_id));
        let metadata = loaded
            .as_ref()
            .map(|shard| ProjectCacheMetadata {
                shard_id: shard_id.to_owned(),
                project_root: shard.project_root.clone(),
                normalized_root: shard.normalized_root.clone(),
                last_used_millis: shard.last_used_millis,
            })
            .or_else(|| self.read_metadata_for_shard(shard_id));
        let cache_path = loaded
            .as_ref()
            .map(|shard| shard.cache_path.clone())
            .unwrap_or_else(|| self.cache_path_for_shard(shard_id));

        if let Some(shard) = loaded {
            shard.cache.clear();
        }

        let project_root = metadata
            .as_ref()
            .map(|metadata| metadata.project_root.clone())
            .unwrap_or_default();
        let cache_path_text = cache_path.to_string_lossy().to_string();

        if metadata.is_none() && !cache_path.exists() {
            return CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: false,
                error: Some("cache shard not found".to_owned()),
            };
        }

        if cache_path.as_os_str().is_empty() {
            return CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: true,
                error: None,
            };
        }

        match fs::remove_dir_all(&cache_path) {
            Ok(()) => CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: true,
                error: None,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: true,
                error: None,
            },
            Err(error) => CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: false,
                error: Some(error.to_string()),
            },
        }
    }

    fn info_for_loaded(&self, shard_id: &str, shard: &LoadedProjectCache) -> CacheShardInfo {
        CacheShardInfo {
            shard_id: shard_id.to_owned(),
            project_root: shard.project_root.clone(),
            normalized_root: shard.normalized_root.clone(),
            cache_path: shard.cache_path.to_string_lossy().to_string(),
            size_bytes: directory_size(&shard.cache_path),
            last_used_millis: Some(shard.last_used_millis),
            loaded: true,
            // Populated from the C1 rollup by `list_shards_with_rollups`.
            entry_count: 0,
        }
    }

    fn scan_disk_shards(&self) -> Vec<CacheShardInfo> {
        let Some(base_path) = self.base_path.as_ref() else {
            return Vec::new();
        };

        let entries = match fs::read_dir(base_path) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let cache_path = entry.path();
                if !cache_path.is_dir() {
                    return None;
                }
                let metadata_path = cache_path.join(SHARD_METADATA_FILE_NAME);
                let metadata = read_metadata(&metadata_path)?;

                Some(CacheShardInfo {
                    shard_id: metadata.shard_id,
                    project_root: metadata.project_root,
                    normalized_root: metadata.normalized_root,
                    cache_path: cache_path.to_string_lossy().to_string(),
                    size_bytes: directory_size(&cache_path),
                    last_used_millis: Some(metadata.last_used_millis),
                    loaded: false,
                    // Populated from the C1 rollup by `list_shards_with_rollups`.
                    entry_count: 0,
                })
            })
            .collect()
    }

    /// Like `scan_disk_shards` but returns only each shard's id and path,
    /// skipping the recursive directory-size walk that invalidation never uses.
    fn scan_disk_shard_paths(&self) -> Vec<(String, PathBuf)> {
        let Some(base_path) = self.base_path.as_ref() else {
            return Vec::new();
        };

        let entries = match fs::read_dir(base_path) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let cache_path = entry.path();
                if !cache_path.is_dir() {
                    return None;
                }
                let metadata = read_metadata(&cache_path.join(SHARD_METADATA_FILE_NAME))?;
                Some((metadata.shard_id, cache_path))
            })
            .collect()
    }

    fn write_metadata_for_loaded(&self, shard_id: &str, shard: &LoadedProjectCache) {
        if let Some((path, metadata)) = self.metadata_write_for_loaded(shard_id, shard) {
            let _ = write_metadata(&path, &metadata);
        }
    }

    // Builds the metadata write target (path + payload) without performing the
    // I/O, so callers on the hot path can capture it under the shards lock and
    // then release the lock before the `fs::write`.
    fn metadata_write_for_loaded(
        &self,
        shard_id: &str,
        shard: &LoadedProjectCache,
    ) -> Option<(PathBuf, ProjectCacheMetadata)> {
        if !self.storage_enabled() {
            return None;
        }

        let metadata = ProjectCacheMetadata {
            shard_id: shard_id.to_owned(),
            project_root: shard.project_root.clone(),
            normalized_root: shard.normalized_root.clone(),
            last_used_millis: shard.last_used_millis,
        };
        Some((shard.cache_path.join(SHARD_METADATA_FILE_NAME), metadata))
    }

    fn read_metadata_for_shard(&self, shard_id: &str) -> Option<ProjectCacheMetadata> {
        let cache_path = self.cache_path_for_shard(shard_id);
        read_metadata(&cache_path.join(SHARD_METADATA_FILE_NAME))
    }

    fn disk_cache_path(&self, cache_path: &Path) -> Option<PathBuf> {
        self.storage_enabled().then(|| cache_path.to_path_buf())
    }

    fn cache_path_for_shard(&self, shard_id: &str) -> PathBuf {
        self.base_path
            .as_ref()
            .filter(|_| self.storage_enabled())
            .map(|base_path| base_path.join(shard_id))
            .unwrap_or_default()
    }

    fn storage_enabled(&self) -> bool {
        self.enable_disk_cache && self.base_path.is_some()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/project_cache_lifecycle.rs"]
mod project_cache_lifecycle_tests;

#[cfg(test)]
#[path = "../../tests/unit/project_cache_maintenance.rs"]
mod project_cache_maintenance_tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCacheStatus {
    pub total_size_bytes: u64,
    /// Σ of every shard's logical (envelope) bytes from the C1 rollups — the
    /// budget-tracked total, distinct from `total_size_bytes` (physical footprint).
    pub total_bytes: u64,
    /// The global disk-byte budget the coordinator enforces (0 disables it).
    pub budget_bytes: u64,
    pub project_count: usize,
    pub max_size_mb: u64,
    pub current_project: Option<CacheShardInfo>,
}

pub fn normalize_project_root(project_root: &Path) -> String {
    let raw = project_root.to_string_lossy().replace('\\', "/");
    let trimmed = raw.trim_end_matches('/').to_owned();

    if cfg!(windows) || trimmed.as_bytes().get(1).is_some_and(|byte| *byte == b':') {
        return trimmed.to_ascii_lowercase();
    }

    trimmed
}

pub fn project_cache_shard_id(project_root: &Path) -> String {
    let normalized = normalize_project_root(project_root);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;

    for byte in normalized.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    format!("v1-{hash:016x}")
}

pub fn remove_legacy_central_cache(storage_path: &Path) -> Option<CacheOperationResult> {
    let cache_path = storage_path.join(LEGACY_CENTRAL_CACHE_DB_FILE_NAME);

    if !cache_path.exists() {
        return None;
    }

    let cache_path_text = cache_path.to_string_lossy().to_string();
    let result = match fs::remove_file(&cache_path) {
        Ok(()) => CacheOperationResult {
            shard_id: LEGACY_CENTRAL_CACHE_SHARD_ID.to_owned(),
            project_root: String::new(),
            cache_path: cache_path_text,
            removed: true,
            error: None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CacheOperationResult {
            shard_id: LEGACY_CENTRAL_CACHE_SHARD_ID.to_owned(),
            project_root: String::new(),
            cache_path: cache_path_text,
            removed: true,
            error: None,
        },
        Err(error) => CacheOperationResult {
            shard_id: LEGACY_CENTRAL_CACHE_SHARD_ID.to_owned(),
            project_root: String::new(),
            cache_path: cache_path_text,
            removed: false,
            error: Some(error.to_string()),
        },
    };

    Some(result)
}

fn should_write_project_metadata(last_write_millis: u64, now_millis: u64) -> bool {
    now_millis.saturating_sub(last_write_millis) >= PROJECT_METADATA_WRITE_INTERVAL_MILLIS
}

fn read_metadata(path: &Path) -> Option<ProjectCacheMetadata> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_metadata(path: &Path, metadata: &ProjectCacheMetadata) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create cache metadata directory: {error}"))?;
    }

    let contents = serde_json::to_string(metadata)
        .map_err(|error| format!("failed to serialize cache metadata: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("failed to write cache metadata: {error}"))
}

fn directory_size(path: &Path) -> u64 {
    if path.as_os_str().is_empty() {
        return 0;
    }

    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };

    if metadata.is_file() {
        return metadata.len();
    }

    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| directory_size(&entry.path()))
        .sum()
}
