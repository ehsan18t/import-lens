use crate::{
    cache::budget::{BudgetCoordinator, EvictableShard, MaintenanceOutcome},
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
/// Minimum wall-clock gap between automatic orphan-shard sweeps. The sweep stats every shard
/// root and abandoned projects are rare, so it runs at most hourly however often projects open.
const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct ProjectCacheRegistry {
    base_path: Option<PathBuf>,
    enable_disk_cache: bool,
    max_size_mb: u64,
    loaded: Mutex<HashMap<String, LoadedProjectCache>>,
    // Per-shard load lock: serializes cold opens of the SAME shard so its database
    // opens exactly once, without holding `loaded` across the open and metadata
    // write. Lock order is always load-lock, then `loaded` (briefly), never the
    // reverse. This map's own mutex is a leaf, held only for the get-or-insert.
    load_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    // Disk-byte budget and cross-shard LRU eviction over every shard under this
    // registry's `base_path`; a 0 budget disables it. FR-026 puts one base per
    // workspace, so the budget is per window, not machine-wide.
    coordinator: BudgetCoordinator,
    // Throttles the automatic orphan sweep to `ORPHAN_SWEEP_INTERVAL`.
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
    /// Set while the shard's disk open has failed; the shard then serves from
    /// memory and retries the open on this backoff.
    disk_retry: Option<DiskRetry>,
}

/// Exponential backoff for retrying a shard's failed disk open, which also bounds
/// the open-failure warnings a permanently unavailable shard logs.
#[derive(Debug, Clone, Copy)]
struct DiskRetry {
    next_attempt_millis: u64,
    interval_millis: u64,
}

impl DiskRetry {
    // Short, because the common failure is this daemon's own maintenance or status pass holding
    // the file for a moment; a permanently unavailable disk still logs at most twice a minute.
    const FIRST_INTERVAL_MILLIS: u64 = 2_000;
    const MAX_INTERVAL_MILLIS: u64 = 30_000;

    fn starting_at(now: u64) -> Self {
        Self {
            next_attempt_millis: now.saturating_add(Self::FIRST_INTERVAL_MILLIS),
            interval_millis: Self::FIRST_INTERVAL_MILLIS,
        }
    }

    /// Claims the retry when it is due, scheduling the next one twice as far out.
    fn claim(&mut self, now: u64) -> bool {
        if now < self.next_attempt_millis {
            return false;
        }
        self.interval_millis = (self.interval_millis * 2).min(Self::MAX_INTERVAL_MILLIS);
        self.next_attempt_millis = now.saturating_add(self.interval_millis);
        true
    }
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

    /// Like `new`, but with an explicit byte budget instead of one derived from
    /// `max_size_mb`.
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

    /// Lifts the process-global recency clock above the maximum persisted seq across
    /// every on-disk shard; must run before the server accepts a request. The clock
    /// restarts at 1 each process, so without this a post-restart access could sort
    /// as older than a prior session's entries and the evictor would pick the active
    /// project over a stale shard.
    ///
    /// No-op when the disk cache is disabled. Each unloaded shard is temp-opened and
    /// its `max_seq` read from a single SUMMARY key, never a CACHE_TABLE scan; loaded
    /// shards are read from their live handles.
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
            let cache = ImportCache::open_existing(cache_path, self.enable_disk_cache);
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

    /// One maintenance pass: byte-budget eviction, then per-shard fragmentation
    /// compaction, then a zero-threshold compaction if physical bytes still exceed
    /// the budget. Both run over loaded shards plus temp-opened unloaded ones, so
    /// the PHYSICAL footprint tracks the budget, not just the logical total.
    ///
    /// Unless `force` is set, the pass is skipped when the summed `.redb` file
    /// sizes (an upper bound on the logical total) are within budget. Queued
    /// inserts not yet flushed are invisible to that gate until a later pass.
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
        // Eviction stops at the logical (value-byte) low-water mark, so key, index and
        // page overhead can leave the files over budget even after compaction.
        let physical_bytes = self.total_shard_file_bytes();
        if !eviction.still_over_budget && physical_bytes > self.coordinator.budget_bytes() {
            crate::logging::log_warn(
                "cache",
                format!(
                    "cache files total {} MB after eviction and compaction, over the {} MB \
                     budget: the budget counts stored values, not key, index or page overhead",
                    physical_bytes / (1024 * 1024),
                    self.coordinator.budget_bytes() / (1024 * 1024)
                ),
            );
        }

        MaintenanceOutcome {
            eviction,
            compacted_shards,
            skipped_under_budget: false,
        }
    }

    /// Loaded shards plus every unloaded on-disk shard, temp-opened. The loaded
    /// snapshot is released before any disk I/O. A temp open racing `cache_for_root`
    /// degrades harmlessly: the temp cache evicts nothing, and the loading side
    /// registers a memory-only shard that retries its disk open.
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
            let cache = Arc::new(ImportCache::open_existing(
                cache_path,
                self.enable_disk_cache,
            ));
            targets.push(ShardTarget { shard_id, cache });
        }
        targets
    }

    /// Sum of every shard's `.redb` file size: a cheap upper bound on the logical
    /// cache total, since values live inside the files.
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

        if let Some((cache, retry_disk)) = self.warm_shard_hit(&shard_id, now, true) {
            if retry_disk {
                self.retry_disk_open(&shard_id, &cache);
            }
            return cache;
        }

        // Cold path. The per-shard load lock gives redb a single open of this shard
        // (no `DatabaseAlreadyOpen` self-race) while other shards load in parallel.
        // `loaded` is never held while taking the load lock.
        let load_lock = self.load_lock_for(&shard_id);
        let _load_guard = load_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Another thread may have loaded the shard while we waited.
        if let Some((cache, _)) = self.warm_shard_hit(&shard_id, now, false) {
            return cache;
        }

        let normalized_root = normalize_project_root(project_root);
        let cache_path = self.cache_path_for_shard(&shard_id);
        let disk_path = self.disk_cache_path(&cache_path);
        let cache = Arc::new(ImportCache::new(disk_path, self.enable_disk_cache));
        // A failed open (another daemon holding the file lock, a permission error,
        // a maintenance pass briefly holding it) still registers the shard, so its
        // memory layer survives between requests, and retries the disk on a
        // backoff. No metadata is written until the disk opens.
        let disk_retry = (self.storage_enabled() && !cache.disk_available())
            .then(|| DiskRetry::starting_at(now));
        let shard = LoadedProjectCache {
            project_root: project_root.to_string_lossy().to_string(),
            normalized_root,
            cache_path,
            cache: Arc::clone(&cache),
            last_used_millis: now,
            last_metadata_write_millis: now,
            disk_retry,
        };
        if shard.disk_retry.is_none() {
            self.write_metadata_for_loaded(&shard_id, &shard);
        }
        // The held load lock means no other cold path inserted this shard, so a plain
        // insert cannot clobber a different Arc. A poisoned lock leaves the shard
        // unregistered; this call is still served and the next one heals.
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded.insert(shard_id, shard);
        }
        cache
    }

    /// The warm path of `cache_for_root`: bumps a loaded shard's last-used time
    /// under `loaded`, then releases it before any throttled metadata write, so a
    /// warm hit never blocks peers on disk I/O. Returns `None` (with `loaded`
    /// released) when the shard is absent.
    ///
    /// With `claim_retry`, also reports whether this caller claimed a due disk-open
    /// retry; the claim advances the backoff, so one caller per interval retries.
    fn warm_shard_hit(
        &self,
        shard_id: &str,
        now: u64,
        claim_retry: bool,
    ) -> Option<(Arc<ImportCache>, bool)> {
        let mut loaded = self.loaded.lock().ok()?;
        let shard = loaded.get_mut(shard_id)?;
        shard.last_used_millis = now;
        let retry_disk = claim_retry
            && shard
                .disk_retry
                .as_mut()
                .is_some_and(|retry| retry.claim(now));
        let pending_metadata = if shard.disk_retry.is_none()
            && should_write_project_metadata(shard.last_metadata_write_millis, now)
        {
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
        Some((cache, retry_disk))
    }

    /// Retries the disk open of a shard registered without one. Under the per-shard
    /// load lock, and only while `cache` is still the registered one, so a removal
    /// that ran in between never sees its directory recreated.
    fn retry_disk_open(&self, shard_id: &str, cache: &Arc<ImportCache>) {
        let load_lock = self.load_lock_for(shard_id);
        let _load_guard = load_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let still_registered = self.loaded.lock().is_ok_and(|loaded| {
            loaded
                .get(shard_id)
                .is_some_and(|shard| Arc::ptr_eq(&shard.cache, cache))
        });
        if !still_registered || !cache.reopen_disk() {
            return;
        }
        let pending_metadata = self.loaded.lock().ok().and_then(|mut loaded| {
            let shard = loaded.get_mut(shard_id)?;
            shard.disk_retry = None;
            shard.last_metadata_write_millis = shard.last_used_millis;
            self.metadata_write_for_loaded(shard_id, shard)
        });
        if let Some((path, metadata)) = pending_metadata {
            let _ = write_metadata(&path, &metadata);
        }
    }

    /// Returns the shard's load lock, creating it on first use. The map mutex is a
    /// leaf held only for the get-or-insert; a poisoned map is recovered so a prior
    /// panic cannot wedge all future loads.
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

    /// Builds the shard list, stamping each `entry_count` from `rollups`, so a
    /// status request can reuse one rollup map for its `total_bytes` too.
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

    /// Per-shard rollups keyed by shard id, over the same targets as
    /// [`Self::collect_shard_targets`]. Each rollup is a few SUMMARY scalars, never
    /// a CACHE_TABLE scan, so status and list stay cheap.
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
        // One rollup pass feeds both `entry_count` and `total_bytes`.
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
    /// the folder is gone). An offline drive, or a shard with no recorded root, is
    /// never orphaned.
    fn shard_root_is_orphaned(&self, shard: &CacheShardInfo) -> bool {
        !shard.project_root.is_empty()
            && crate::cache::key::classify_project_root(Path::new(&shard.project_root))
                == crate::cache::key::ProjectRootState::Orphaned
    }

    /// Manual orphan reclaim ("Remove Orphaned Caches"): removes orphaned shards
    /// (drive-safe) and drops stale or uninstalled entries from surviving shards,
    /// stat-only with no project-tree walk. Returns the removed-shard results and
    /// the count of scrubbed entries; a purge that removes no shard can still have
    /// scrubbed entries, and the UI must not call that "nothing to reclaim".
    pub fn purge_orphans(&self) -> (Vec<CacheOperationResult>, usize) {
        let analyzer_version = crate::cache::key::ANALYZER_VERSION;
        let loaded_ids = self
            .loaded
            .lock()
            .map(|loaded| loaded.keys().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();

        let mut removed = Vec::new();
        let mut scrubbed = 0usize;
        for shard in self.list_shards() {
            if self.shard_root_is_orphaned(&shard) {
                removed.push(self.remove_shard_by_id(&shard.shard_id));
                continue;
            }

            if loaded_ids.contains(&shard.shard_id) {
                // Release `loaded` before the scan and write.
                let cache = self.loaded.lock().ok().and_then(|loaded| {
                    loaded
                        .get(&shard.shard_id)
                        .map(|entry| Arc::clone(&entry.cache))
                });
                if let Some(cache) = cache {
                    scrubbed =
                        scrubbed.saturating_add(cache.purge_orphan_entries(analyzer_version));
                }
            } else if !shard.cache_path.is_empty() {
                let cache = ImportCache::open_existing(
                    PathBuf::from(&shard.cache_path),
                    self.enable_disk_cache,
                );
                scrubbed = scrubbed.saturating_add(cache.purge_orphan_entries(analyzer_version));
            }
        }

        (removed, scrubbed)
    }

    /// Automatic orphan reclaim for the maintenance pass: removes only orphaned
    /// shards (drive-safe) and leaves surviving shards untouched, since stale
    /// entries are reclaimed on access. Returns empty until `ORPHAN_SWEEP_INTERVAL`
    /// has passed since the last sweep.
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

    /// Invalidates every named package across all loaded and on-disk shards, opening
    /// each on-disk shard once.
    pub fn invalidate_packages(&self, package_names: &[String]) {
        if package_names.is_empty() {
            return;
        }

        let package_set: HashSet<String> = package_names.iter().cloned().collect();

        // Snapshot under `loaded`, then release it before the writes: an invalidation
        // must not stall every other project's analysis for an N-shard rewrite. redb
        // serializes each shard's writer, so the writes need no global lock.
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
            let cache = ImportCache::open_existing(cache_path, self.enable_disk_cache);
            cache.invalidate_packages(&package_set);
        }
    }

    pub fn clear_all(&self) {
        let _ = self.remove_all();
    }

    pub fn recent_keys(&self, project_root: &Path, limit: usize) -> Vec<String> {
        self.cache_for_root(project_root).recent_keys(limit)
    }

    pub fn flush_to_disk(&self) -> Result<(), String> {
        // Recover a poisoned lock: every shard that can flush must, so a panic under
        // a brief map op must not skip all flushes.
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
        // The load lock stops a cold opener registering the shard while its
        // directory is deleted. Same order as `cache_for_root`: load lock, then
        // `loaded` briefly.
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

        if let Err(error) = remove_shard_database(&cache_path.join(SHARD_DB_FILE_NAME)) {
            return CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: false,
                error: Some(error),
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
            // Populated from the rollup by `list_shards_with_rollups`.
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
                    // Populated from the rollup by `list_shards_with_rollups`.
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

    // Builds the metadata write without performing it, so a caller can capture it
    // under `loaded` and write after releasing the lock.
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
    /// Sum of every shard's logical (envelope) bytes: the budget-tracked total, as
    /// opposed to the physical footprint in `total_size_bytes`.
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

/// Deletes a shard's database, before anything else in its directory and only when nothing holds
/// it open: redb refuses an open while this process or another one holds the file. On unix an
/// unlink would otherwise succeed under a live holder, whose writes then vanish with the inode.
/// Deleting the database first means a refused or failed delete leaves the metadata sidecar in
/// place, so the shard stays listed instead of turning into a directory no listing shows.
fn remove_shard_database(db_path: &Path) -> Result<(), String> {
    if !db_path.exists() {
        return Ok(());
    }
    let removed = match redb::Database::open(db_path) {
        Ok(database) => {
            // Unix: unlink while still holding it, so no other opener fits between the check and
            // the delete. Windows cannot delete an open file, and an opener there fails the delete.
            #[cfg(unix)]
            let removed = fs::remove_file(db_path);
            drop(database);
            #[cfg(not(unix))]
            let removed = fs::remove_file(db_path);
            removed
        }
        Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
            return Err(
                "the cache shard is still open (another Import Lens window, or a request still \
                 using it); nothing was removed"
                    .to_owned(),
            );
        }
        // Unreadable as a database, so nothing can hold it as one: remove the file itself.
        Err(_) => fs::remove_file(db_path),
    };
    match removed {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
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
    crate::atomic_write::write_atomic(path, contents.as_bytes())
        .map_err(|error| format!("failed to write cache metadata: {error}"))
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
