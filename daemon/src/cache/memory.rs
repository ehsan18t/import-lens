use crate::{
    cache::{
        disk::DiskCache,
        key::{
            FileFingerprint, Freshness, cache_key_is_orphan, cache_key_matches_any_package,
            check_fingerprints_strict, fingerprints_are_reusable,
        },
        recency::RecencyClock,
    },
    ipc::protocol::{ImportResult, ResultFreshness},
};
use papaya::HashMap;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub const RECENT_PRELOAD_LIMIT: usize = 20;

// The in-memory map is capped by entry count and by approximate size, because an
// entry's fingerprint and contribution lists grow with the measured package's graph
// (up to thousands of modules, each with an absolute path). Eviction drops the
// least-recently-used entry; its disk copy (if any) re-hydrates on the next hit.
pub const MAX_MEMORY_ENTRIES: usize = 4096;
const MAX_MEMORY_WEIGHT_BYTES: usize = 128 * 1024 * 1024;
/// Rough heap cost of one fingerprint or contribution row: the struct plus a typical
/// absolute module path (pnpm store paths run 150-250 bytes).
const GRAPH_ROW_WEIGHT_BYTES: usize = 256;

fn memory_weight(cached: &CachedImport) -> usize {
    (cached.dependency_fingerprints.len() + cached.result.internal_contributions.len())
        .saturating_add(1)
        .saturating_mul(GRAPH_ROW_WEIGHT_BYTES)
}

// node_modules dependencies change only with node_modules, which the extension
// signals via cache invalidation, so an entry verified at the current generation
// skips the re-stat. The TTL backstops a node_modules change with no invalidation
// event (a watcher-excluded folder).
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(1);
pub(crate) const REVERIFY_TTL: Duration = Duration::from_secs(30);

pub fn bump_cache_generation() {
    CACHE_GENERATION.fetch_add(1, Ordering::Release);
}

/// Serializes the lib tests that touch [`CACHE_GENERATION`] against the ones that assume it holds
/// still while they run.
///
/// The generation is process-global and the test binary is multi-threaded, so a bump can land
/// between another test's two reads of it, turning a single-flight follower into its own leader
/// (`service::analyze_and_cache` re-reads the generation per call) or making two L1 signatures
/// disagree.
#[cfg(test)]
pub(crate) fn hold_cache_generation_steady() -> std::sync::MutexGuard<'static, ()> {
    static GENERATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GENERATION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
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
    // Runtime verification state (not persisted): the generation and monotonic
    // instant at which this entry's fingerprints were last confirmed current.
    pub verified_generation: u64,
    // `None` = never verified this run (fresh decode from disk): the next read
    // re-verifies. Monotonic so a backward clock jump cannot extend the window.
    pub verified_at: Option<Instant>,
    // Recency sequence of the last interactive hit; drives LRU eviction for both
    // the memory working set and the disk byte budget. Shared via Arc so a hit
    // bumps it in place; persisted as a plain `u64` at flush time.
    pub last_seq: Arc<AtomicU64>,
    // The `last_seq` the disk layer currently holds for this entry.
    // `last_seq > persisted_seq` means used since last persisted: the eviction
    // filter treats it as hot and the flush sweep re-persists it.
    pub persisted_seq: Arc<AtomicU64>,
    // Whether the key resolves to a first-party dependency (workspace, npm link,
    // `file:`). Memoized because deriving it decodes the key, too costly per hit.
    pub first_party: bool,
}

// A transient stat/read error on a dependency (`Freshness::Unknown`, e.g. a file
// locked briefly by a save or an AV scan) is graduated: the first sightings serve the
// last value quietly flagged `Stale{revalidating}`, and it surfaces as `Unverified`
// only once the error persists past either bound.
const UNKNOWN_MAX_ATTEMPTS: u32 = 3;
const UNKNOWN_PERSIST_AFTER: Duration = Duration::from_secs(2);

/// Per-key graduation state for a transient `Unknown` freshness. Lives only while an
/// entry is mid-graduation (the slow re-check path); cleared on any non-`Unknown`
/// outcome and on re-insert, so a later blip starts a fresh window.
#[derive(Debug, Clone, Copy)]
struct UnknownRetry {
    // Monotonic, never `SystemTime`, so a backward clock jump cannot stretch or
    // collapse the persistence window.
    first_seen: Instant,
    attempts: u32,
}

#[derive(Debug)]
pub struct ImportCache {
    memory: HashMap<String, CachedImport>,
    disk: DiskCache,
    // Keys whose synchronous disk insert failed; flush_to_disk replays these.
    dirty: Mutex<HashSet<String>>,
    // Background SWR revalidation claims in flight. Callers choose the claim shape:
    // same-document work can coalesce, while independent documents may use distinct
    // claims even when they refresh the same cache key.
    revalidating: Mutex<HashSet<String>>,
    // Per-key `Unknown` graduation windows, removed on any non-`Unknown` outcome and
    // on re-insert. Independent of the papaya `memory` map.
    unknown_retry: Mutex<std::collections::HashMap<String, UnknownRetry>>,
}

impl Default for ImportCache {
    fn default() -> Self {
        Self {
            memory: HashMap::new(),
            disk: DiskCache::default(),
            dirty: Mutex::new(HashSet::new()),
            revalidating: Mutex::new(HashSet::new()),
            unknown_retry: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// RAII claim on an in-flight revalidation. Released on drop, so a recompute that
/// panics cannot leak the claim and block that key's revalidation until restart.
#[must_use = "dropping the guard immediately releases the revalidation claim"]
pub struct RevalidationGuard<'cache> {
    cache: &'cache ImportCache,
    key: String,
}

impl Drop for RevalidationGuard<'_> {
    fn drop(&mut self) {
        self.cache.finish_revalidation(&self.key);
    }
}

/// Read semantics for the shared cache-read path (`ImportCache::read`).
#[derive(Clone, Copy)]
enum ReadIntent {
    /// Bulk read: serves the last-known value even on a transient `Unknown`, and
    /// never promotes LRU recency (scan resistance).
    Serve,
    /// Force-fresh read: serves only a value verified `Fresh` against disk. Every
    /// other state yields `None` so the caller recomputes; an `Unknown` entry is kept
    /// (never deleted, served, or hydrated). Promotes recency when `promote` is set.
    RequireFresh { promote: bool },
}

impl ReadIntent {
    fn promotes(self) -> bool {
        match self {
            Self::Serve => false,
            Self::RequireFresh { promote } => promote,
        }
    }
}

/// What a read does with an entry whose dependencies changed but still exist.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StalePolicy {
    /// Drop it from both layers so the caller recomputes.
    Evict,
    /// Keep serving it, flagged stale, while a background recompute replaces it.
    Serve,
}

impl ImportCache {
    pub fn new(storage_path: Option<PathBuf>, enable_disk_cache: bool) -> Self {
        Self::new_with_recent_preload_limit(storage_path, enable_disk_cache, RECENT_PRELOAD_LIMIT)
    }

    /// A cache over a shard that must already exist (see `DiskCache::open_existing`),
    /// with no recent-entry preload: for maintenance and observability passes.
    pub fn open_existing(storage_path: PathBuf, enable_disk_cache: bool) -> Self {
        Self::with_disk(
            DiskCache::open_existing(Some(storage_path), enable_disk_cache),
            0,
        )
    }

    pub fn new_with_recent_preload_limit(
        storage_path: Option<PathBuf>,
        enable_disk_cache: bool,
        recent_preload_limit: usize,
    ) -> Self {
        Self::with_disk(
            DiskCache::new(storage_path, enable_disk_cache),
            recent_preload_limit,
        )
    }

    fn with_disk(disk: DiskCache, recent_preload_limit: usize) -> Self {
        let memory = HashMap::new();
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
            revalidating: Mutex::new(HashSet::new()),
            unknown_retry: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Bulk/prewarm read: does not promote recency, so a workspace scan or a
    /// prefetcher dedup check cannot evict the user's warm working set.
    pub fn get_for_prewarm(&self, key: &str) -> Option<ImportResult> {
        self.read(key, ReadIntent::Serve)
    }

    /// Force-fresh read (CI / `importlens check`): returns the cached value only when
    /// it is verified `Fresh` against disk, in memory or on disk, so CI never serves
    /// a stale or unverified value:
    ///   Fresh: Some(value). Stale/Gone: evict, None.
    ///   Unknown: keep, but None. Miss: None.
    /// Does not promote recency (a workspace report is a bulk read). Classified by the
    /// same `lookup` as the normal read; only the serve-on-`Unknown` decision differs.
    pub fn get_if_fresh(&self, key: &str) -> Option<ImportResult> {
        self.read(key, ReadIntent::RequireFresh { promote: false })
    }

    /// `get_if_fresh` for an interactive read: the same freshness gate, and a hit
    /// promotes the entry's recency (FR-026b).
    pub fn get_if_fresh_and_promote(&self, key: &str) -> Option<ImportResult> {
        self.read(key, ReadIntent::RequireFresh { promote: true })
    }

    /// Read for `get_for_prewarm` (serve) and `get_if_fresh` (force-fresh): a
    /// stale entry is evicted, and a force-fresh read refuses an unverified one.
    fn read(&self, key: &str, intent: ReadIntent) -> Option<ImportResult> {
        let require_fresh = matches!(intent, ReadIntent::RequireFresh { .. });
        let (result, freshness) =
            self.lookup(key, intent.promotes(), require_fresh, StalePolicy::Evict)?;
        (!require_fresh || freshness == Freshness::Fresh).then_some(result)
    }

    /// Stale-while-revalidate read: like `get_for_prewarm`, but on `Stale` it serves the last-known
    /// value flagged `Stale { revalidating: true }` so the caller can answer instantly
    /// and recompute in the background. `Gone` evicts and returns `None`; a transient
    /// `Unknown` is served as `Stale { revalidating: true }` and surfaces as
    /// `Unverified` once it persists past the window. Dedupe is the caller's, via
    /// `begin_revalidation`. A hit always promotes recency.
    pub fn get_with_result_freshness(&self, key: &str) -> Option<(ImportResult, ResultFreshness)> {
        let (mut result, freshness) = self.lookup(key, true, false, StalePolicy::Serve)?;
        let served = match freshness {
            Freshness::Fresh => ResultFreshness::fresh(),
            Freshness::Stale => ResultFreshness::stale(true),
            Freshness::Unknown | Freshness::Gone => self.record_unknown(key),
        };
        result.freshness = served.clone();
        Some((result, served))
    }

    /// The one classify-and-serve mechanism behind every read: the memory working
    /// set first, then the disk shard. Returns the result (`cache_hit` set) with the
    /// freshness it was served under: `Fresh`, `Unknown` (a transient error; the
    /// entry is kept and not restamped, so the next read re-checks), or `Stale`
    /// (only under `StalePolicy::Serve`). `Gone` always evicts, as does `Stale`
    /// under `StalePolicy::Evict`. `require_fresh` skips the TTL fast path and never
    /// hydrates an `Unknown` disk entry; `promote` bumps LRU recency on a hit.
    fn lookup(
        &self,
        key: &str,
        promote: bool,
        require_fresh: bool,
        stale: StalePolicy,
    ) -> Option<(ImportResult, Freshness)> {
        let memory = self.memory.pin();
        if let Some(cached) = memory.get(key) {
            // The Arc is shared with the restamp clone below, so the bump survives it.
            if promote {
                cached
                    .last_seq
                    .store(RecencyClock::next_seq(), Ordering::Relaxed);
            }
            let generation = current_cache_generation();
            // A force-fresh read never rides the TTL fast path: a node_modules change
            // with no generation bump (a watcher-excluded folder) would be served
            // unverified inside the window. First-party deps change without any
            // generation bump, so they always re-verify.
            let fast_path = !require_fresh
                && !cached.first_party
                && cached.verified_generation == generation
                && cached
                    .verified_at
                    .is_some_and(|at| at.elapsed() < REVERIFY_TTL);
            // Hash-verified per fingerprint, never per entry: a node_modules entry can
            // carry a workspace file that a stylesheet's `url()` reached outside the
            // package root (D18), and that file changes with no generation bump.
            let freshness = if fast_path {
                Freshness::Fresh
            } else {
                check_fingerprints_strict(&cached.dependency_fingerprints)
            };
            match freshness {
                Freshness::Unknown => {}
                Freshness::Stale if stale == StalePolicy::Serve => self.clear_unknown(key),
                Freshness::Stale | Freshness::Gone => {
                    self.clear_unknown(key);
                    self.evict_if_current(key, &cached.last_seq);
                    return None;
                }
                Freshness::Fresh if fast_path => {}
                Freshness::Fresh => {
                    self.clear_unknown(key);
                    // `update` is a no-op when the key was concurrently removed, so a
                    // racing invalidation or clear is never resurrected. First-party
                    // entries never consult the stamps, so they skip the clone.
                    if !cached.first_party {
                        memory.update(key.to_owned(), |entry| {
                            let mut restamped = entry.clone();
                            restamped.verified_generation = generation;
                            restamped.verified_at = Some(Instant::now());
                            restamped
                        });
                    }
                }
            }
            let mut result = cached.result.clone();
            result.cache_hit = true;
            return Some((result, freshness));
        }
        // Release the pin before the disk probe: it stats and may read files, and an
        // epoch guard held across that I/O delays reclamation of removed entries.
        drop(memory);

        // Both generations are captured BEFORE the disk probe. Stamping a generation
        // read after it would launder an entry invalidated mid-probe into "verified"
        // for the whole TTL, and a `clear()` racing the hydration below must roll the
        // memory copy back.
        let hydration_generation = current_cache_generation();
        let clear_generation = self.disk.clear_generation();
        // The disk layer evicts Stale/Gone itself, so this is Fresh or Unknown.
        let (mut cached, freshness) = self.disk.get_with_freshness(key)?;
        if freshness == Freshness::Fresh {
            cached.verified_generation = hydration_generation;
            cached.verified_at = Some(Instant::now());
            self.clear_unknown(key);
        } else if require_fresh {
            // Kept on disk, but neither served nor hydrated.
            return None;
        }
        // An `Unknown` keeps the decoded "never verified" stamps, so the next read
        // re-checks. An interactive hit promotes even here, or a just-used rehydrated
        // entry keeps its old persisted seq and is a prime eviction victim.
        if promote {
            cached
                .last_seq
                .store(RecencyClock::next_seq(), Ordering::Relaxed);
        }
        let mut result = cached.result.clone();
        result.cache_hit = true;
        self.insert_into_memory_guarded(key.to_owned(), cached, clear_generation);
        self.enforce_memory_cap();
        Some((result, freshness))
    }

    /// Claim ownership of a background revalidation. Returns `Some(guard)` for the
    /// first caller (which should spawn the recompute) and `None` while that claim is
    /// already in flight. The guard releases the claim on drop, including on unwind.
    pub fn begin_revalidation(&self, claim_key: &str) -> Option<RevalidationGuard<'_>> {
        let mut inflight = self
            .revalidating
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inflight.insert(claim_key.to_owned()) {
            Some(RevalidationGuard {
                cache: self,
                key: claim_key.to_owned(),
            })
        } else {
            None
        }
    }

    /// Release an in-flight claim; called by `RevalidationGuard` on drop.
    fn finish_revalidation(&self, claim_key: &str) {
        let mut inflight = self
            .revalidating
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inflight.remove(claim_key);
    }

    /// Records another transient `Unknown` sighting for `key` and returns the serve-time
    /// freshness: `Stale { revalidating: true }` within `UNKNOWN_MAX_ATTEMPTS` sightings
    /// and `UNKNOWN_PERSIST_AFTER`, then `Unverified { reason }`. Never deletes and never
    /// claims `Fresh`.
    fn record_unknown(&self, key: &str) -> ResultFreshness {
        let mut retries = self
            .unknown_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = retries.entry(key.to_owned()).or_insert(UnknownRetry {
            first_seen: Instant::now(),
            attempts: 0,
        });
        entry.attempts = entry.attempts.saturating_add(1);
        let within_window = entry.attempts <= UNKNOWN_MAX_ATTEMPTS
            && entry.first_seen.elapsed() < UNKNOWN_PERSIST_AFTER;
        if within_window {
            ResultFreshness::stale(true)
        } else {
            ResultFreshness::unverified("dependency verification failed (transient)")
        }
    }

    /// Clears any `Unknown` graduation window for `key`. Called on every non-`Unknown`
    /// outcome and on re-insert, so a later transient error starts a fresh window
    /// instead of surfacing `Unverified` at once.
    fn clear_unknown(&self, key: &str) {
        let mut retries = self
            .unknown_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retries.remove(key);
    }

    /// Re-probe the raw dependency freshness of a memory-resident `key` without
    /// serving or restamping it. Background SWR uses this to tell a genuinely `Stale`
    /// entry (recompute) from one served as `Stale` because of a transient `Unknown`,
    /// which must never recompute: re-analyzing would re-hit the same error and could
    /// overwrite the good value with an error result. Bypasses the TTL fast path.
    /// Returns `None` when the key is not in the memory working set.
    pub fn probe_freshness(&self, key: &str) -> Option<Freshness> {
        let memory = self.memory.pin();
        let cached = memory.get(key)?;
        // Per fingerprint, not per entry (see `lookup`, D18).
        let freshness = check_fingerprints_strict(&cached.dependency_fingerprints);
        Some(freshness)
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
        self.insert_with_fingerprints_at_generation(
            key,
            result,
            dependency_fingerprints,
            current_cache_generation(),
        );
    }

    /// Insert stamping a caller-captured generation (taken BEFORE reading the
    /// analyzed bytes) rather than the generation at insert time. If an
    /// invalidation bumped the generation during analysis, the entry is born
    /// "must re-verify" and cannot be served on the fast path.
    ///
    /// **The transience gate lives here**, in the store, not at the call sites (ADR-0006,
    /// invariant 3), so no caller can write a timed-out build into L1 or L2 by forgetting it.
    pub fn insert_with_fingerprints_at_generation(
        &self,
        key: String,
        result: ImportResult,
        dependency_fingerprints: Vec<FileFingerprint>,
        verified_generation: u64,
    ) {
        if !result.is_durable() || !fingerprints_are_reusable(&dependency_fingerprints) {
            // A refused failure (timeout, locked file) is routine: debug. A refused
            // measurement means `pipeline::stage` has not classified a stage, which costs this
            // package its cache forever, so it warns.
            let stage = result.unmeasured_stage().unwrap_or("none");
            if result.sizes().is_some() {
                crate::logging::log_warn(
                    "cache",
                    format!(
                        "refusing to cache a MEASURED result for {key}: a diagnostic carries a \
                         stage no durable store accepts (see pipeline::stage)"
                    ),
                );
            } else {
                crate::logging::log_debug(
                    "cache",
                    format!("refusing to cache a non-durable result for {key} (stage: {stage})"),
                );
            }
            return;
        }
        self.clear_unknown(&key);
        // Capture the clear generation BEFORE the disk enqueue and memory insert, and
        // tag both with it, so a `clear()` racing this insert drops the disk copy at
        // flush and rolls back the memory copy: the two never diverge.
        let clear_generation = self.disk.clear_generation();
        let born_seq = RecencyClock::next_seq();
        let cached = CachedImport {
            result,
            dependency_fingerprints,
            verified_generation,
            verified_at: Some(Instant::now()),
            last_seq: Arc::new(AtomicU64::new(born_seq)),
            // The disk insert below persists this same seq.
            persisted_seq: Arc::new(AtomicU64::new(born_seq)),
            first_party: crate::cache::key::cache_key_is_first_party(&key),
        };

        if let Err(error) = self
            .disk
            .insert_at_generation(&key, &cached, clear_generation)
        {
            crate::logging::log_warn("cache", format!("skipping disk insert for {key}: {error}"));
            if let Ok(mut dirty) = self.dirty.lock() {
                dirty.insert(key.clone());
            }
        }

        self.insert_into_memory_guarded(key, cached, clear_generation);
        self.enforce_memory_cap();
    }

    /// Evicts the least-recently-used entries while the in-memory map is over either
    /// cap; the disk copy re-hydrates on the next hit. Called from every path that
    /// grows the map (insert and disk re-hydration).
    ///
    /// Evicts in one batch down to ~90% of both caps, so a session pinned at a cap
    /// pays one sort per batch instead of a scan per insert. `dirty` entries (disk
    /// insert failed) are never evicted: they exist only in memory until
    /// `flush_to_disk` replays them.
    fn enforce_memory_cap(&self) {
        let memory = self.memory.pin();
        let weight = memory
            .values()
            .map(memory_weight)
            .fold(0usize, usize::saturating_add);
        if memory.len() <= MAX_MEMORY_ENTRIES && weight <= MAX_MEMORY_WEIGHT_BYTES {
            return;
        }

        // Captured before the candidate snapshot: a `clear()` landing mid-eviction bumps
        // the generation, and the flush filter drops the re-persist below instead of
        // resurrecting the shard.
        let clear_generation = self.disk.clear_generation();
        let dirty = self
            .dirty
            .lock()
            .map(|dirty| dirty.clone())
            .unwrap_or_default();
        let mut candidates = memory
            .iter()
            .filter(|(key, _)| !dirty.contains(*key))
            .map(|(key, entry)| {
                (
                    entry.last_seq.load(Ordering::Relaxed),
                    key.clone(),
                    memory_weight(entry),
                )
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable();

        let mut count = memory.len();
        let mut weight = weight;
        for (_, key, entry_weight) in candidates {
            if count <= MAX_MEMORY_ENTRIES * 9 / 10 && weight <= MAX_MEMORY_WEIGHT_BYTES * 9 / 10 {
                break;
            }
            count -= 1;
            weight = weight.saturating_sub(entry_weight);
            // Persist any unflushed recency promotion before dropping the mirror: once
            // the entry leaves memory, `flush_to_disk`'s sweep cannot reach it, and the
            // disk copy's old seq would make a just-used entry a prime disk victim.
            if let Some(cached) = memory.get(&key) {
                let last_seq = cached.last_seq.load(Ordering::Relaxed);
                if last_seq > cached.persisted_seq.load(Ordering::Relaxed)
                    && self
                        .disk
                        .insert_at_generation(&key, cached, clear_generation)
                        .is_ok()
                {
                    cached.persisted_seq.store(last_seq, Ordering::Relaxed);
                }
            }
            memory.remove(&key);
        }
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
        // disk.clear() bumps the clear generation BEFORE it wipes; the memory-insert
        // paths captured that generation before their insert and roll back if it moved,
        // so an insert racing this clear cannot leave a memory-only survivor.
        self.disk.clear();
        self.memory.pin().clear();
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.clear();
        }
        if let Ok(mut retries) = self.unknown_retry.lock() {
            retries.clear();
        }
    }

    /// Inserts into the memory mirror, guarding against a racing `clear()`. A change
    /// from `captured_generation` (read before the caller derived `cached`) means a
    /// wipe may have run since, and the insert would leave a memory-only survivor of a
    /// cleared cache. The caller passes the same generation it tagged the paired
    /// `disk.insert_at_generation` with, so both copies live or die together. Does not
    /// enforce the memory cap.
    fn insert_into_memory_guarded(
        &self,
        key: String,
        cached: CachedImport,
        captured_generation: u64,
    ) {
        // Skip outright when already superseded: a stale insert could clobber a
        // concurrent post-clear insert of the same key.
        if self.disk.clear_generation() != captured_generation {
            return;
        }
        // The rollback removes only our own entry (by its unique last_seq Arc), never
        // a post-clear insert of the same key that replaced it.
        let our_last_seq = Arc::clone(&cached.last_seq);
        let memory = self.memory.pin();
        memory.insert(key.clone(), cached);
        // A clear() whose wipe preceded our insert still rolls us back.
        if self.disk.clear_generation() != captured_generation {
            self.remove_from_memory_if_current(key, &our_last_seq);
        }
    }

    /// Evicts the entry a read judged Stale or Gone from both layers, unless a
    /// recompute replaced it while it was being verified: the verdict is about the
    /// old measurement only. The disk copy goes only with the memory one; if memory
    /// no longer held the entry at all, a later disk read re-verifies it anyway.
    fn evict_if_current(&self, key: &str, identity: &Arc<AtomicU64>) {
        if self.remove_from_memory_if_current(key.to_owned(), identity) {
            self.disk.remove(key);
        }
    }

    /// Removes `key` from memory only while it still holds the entry identified by
    /// `identity` (its `last_seq` Arc, unique per insert and shared by a restamp, so
    /// a restamped copy of the same measurement still matches). Returns whether it
    /// removed anything.
    fn remove_from_memory_if_current(&self, key: String, identity: &Arc<AtomicU64>) -> bool {
        matches!(
            self.memory.pin().compute(key, |entry| match entry {
                Some((_, current)) if Arc::ptr_eq(&current.last_seq, identity) => {
                    papaya::Operation::Remove
                }
                _ => papaya::Operation::Abort(()),
            }),
            papaya::Compute::Removed(..)
        )
    }

    pub fn memory_len(&self) -> usize {
        self.memory.pin().len()
    }

    pub fn recent_keys(&self, limit: usize) -> Vec<String> {
        self.disk.recent_keys(limit)
    }

    /// Whether the disk layer is actually open. False when disk caching is
    /// disabled or the database open failed (see `DiskCache::is_available`).
    pub fn disk_available(&self) -> bool {
        self.disk.is_available()
    }

    /// Attaches the shard's disk if it is missing (see `DiskCache::reopen_if_unavailable`). On
    /// the transition, entries computed while it was missing are written (they were never
    /// queued), and the recent-entry preload runs for every key memory does not already hold.
    pub fn reopen_disk(&self) -> bool {
        if self.disk.is_available() {
            return true;
        }
        if !self.disk.reopen_if_unavailable() {
            return false;
        }

        let clear_generation = self.disk.clear_generation();
        let failed = {
            let memory = self.memory.pin();
            memory
                .iter()
                .filter(|(key, cached)| {
                    self.disk
                        .insert_at_generation(key, cached, clear_generation)
                        .is_err()
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>()
        };
        if !failed.is_empty()
            && let Ok(mut dirty) = self.dirty.lock()
        {
            dirty.extend(failed);
        }

        for (key, cached) in self.disk.load_recent(RECENT_PRELOAD_LIMIT) {
            if !self.memory.pin().contains_key(&key) {
                self.insert_into_memory_guarded(key, cached, clear_generation);
            }
        }
        self.enforce_memory_cap();
        true
    }

    /// One-pass byte/recency summary of this cache's disk shard for the capacity
    /// coordinator. Empty when the disk cache is disabled.
    pub fn shard_rollup(&self) -> crate::cache::disk::ShardRollup {
        self.disk.shard_rollup()
    }

    /// The largest persisted recency seq in this shard's disk layer, a single-key
    /// summary read for the startup recency seed. `0` when the disk cache is
    /// disabled. See `DiskCache::summary_max_seq`.
    pub fn summary_max_seq(&self) -> u64 {
        self.disk.summary_max_seq()
    }

    /// Up to `n` cold eviction victims: the shard's lowest-persisted-seq keys beyond
    /// its `floor` newest, skipping memory-hot ones. Used by the byte-budget evictor.
    ///
    /// An entry is memory-hot when its in-memory `last_seq` is promoted past the
    /// persisted seq the disk index sorted it by (an interactive hit bumps only
    /// `last_seq` until `flush_to_disk`). It was used since it was last persisted and
    /// is never a correct victim.
    ///
    /// The lowest-`n` batch can be entirely hot while cold entries sit deeper, so this
    /// pages past hot keys until it collects `n` cold keys or exhausts the evictable
    /// region, bounded by `MAX_EVICTION_SCAN`: an all-hot prefix returns empty and the
    /// evictor retires the shard.
    pub fn lowest_seq_disk_keys(&self, n: usize, floor: u64) -> Vec<String> {
        if n == 0 {
            return Vec::new();
        }

        // Fast path: the `n` lowest-persisted-seq keys, usually none memory-hot.
        let first = self.disk.lowest_seq_keys(n, floor);
        let region_exhausted = first.len() < n;
        let cold = self.filter_evictable(first, n);
        if cold.len() == n || region_exhausted {
            // Batch filled, or the evictable region held fewer than `n` keys.
            return cold;
        }

        // Some were memory-hot and the region extends past them: page a bounded wider
        // window. Filling a full batch relies on `n <= MAX_EVICTION_SCAN`; the sole
        // caller passes `n = EVICTION_BATCH` and the window is `8 * EVICTION_BATCH`.
        let wide = self
            .disk
            .lowest_seq_keys(crate::cache::budget::MAX_EVICTION_SCAN, floor);
        self.filter_evictable(wide, n)
    }

    /// Collects up to `n` keys that are not memory-hot from ascending
    /// `(key, persisted_seq)` candidates, preserving their order.
    fn filter_evictable(&self, candidates: Vec<(String, u64)>, n: usize) -> Vec<String> {
        let memory = self.memory.pin();
        let mut cold = Vec::with_capacity(n.min(candidates.len()));
        for (key, persisted_seq) in candidates {
            let memory_hot = memory
                .get(&key)
                .is_some_and(|entry| entry.last_seq.load(Ordering::Relaxed) > persisted_seq);
            if !memory_hot {
                cold.push(key);
                if cold.len() >= n {
                    break;
                }
            }
        }
        cold
    }

    /// Evicts `keys` from both the disk shard and the in-memory mirror, returning
    /// the on-disk bytes freed.
    pub fn evict_keys(&self, keys: &[String]) -> u64 {
        let freed = self.disk.remove_keys(keys);
        let memory = self.memory.pin();
        for key in keys {
            memory.remove(key);
        }
        freed
    }

    /// Compacts the disk shard's redb file when its free-space ratio exceeds
    /// `threshold`. Off the hot path (idle maintenance). Returns whether it ran.
    pub fn compact_if_fragmented(&self, threshold: f64) -> bool {
        self.disk.compact_if_fragmented(threshold)
    }

    // Inserts are queued in the disk cache for batched commit; a recycle must
    // drain that queue. Any entry whose enqueue failed is marked dirty and
    // re-enqueued here before the queue is flushed.
    pub fn flush_to_disk(&self) -> Result<(), String> {
        // Captured BEFORE snapshotting memory: a `clear()` landing between the snapshot
        // and the enqueue bumps the generation, and the flush filter drops these writes
        // instead of resurrecting the wiped shard.
        let clear_generation = self.disk.clear_generation();
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

        let mut errors = Vec::new();
        let mut failed_dirty = HashSet::new();
        for (key, cached) in entries {
            if let Err(error) = self
                .disk
                .insert_at_generation(&key, &cached, clear_generation)
            {
                failed_dirty.insert(key.clone());
                errors.push(format!("{key}: {error}"));
            }
        }

        // Recency sweep: re-persist every entry promoted since its last persist so
        // session recency survives a restart (within a session,
        // `lowest_seq_disk_keys` shields hot entries).
        let promoted = {
            let memory = self.memory.pin();
            memory
                .iter()
                .filter(|(key, cached)| {
                    !failed_dirty.contains(*key)
                        && cached.last_seq.load(Ordering::Relaxed)
                            > cached.persisted_seq.load(Ordering::Relaxed)
                })
                .map(|(key, cached)| (key.clone(), cached.clone()))
                .collect::<Vec<_>>()
        };
        for (key, cached) in promoted {
            // Capture the seq BEFORE the insert: a concurrent promotion mid-flush must
            // leave `persisted_seq` at or behind what disk holds. Behind re-persists next
            // flush; ahead would hide the promotion from future sweeps.
            let seq_at_flush = cached.last_seq.load(Ordering::Relaxed);
            match self
                .disk
                .insert_at_generation(&key, &cached, clear_generation)
            {
                Ok(()) => cached.persisted_seq.store(seq_at_flush, Ordering::Relaxed),
                Err(error) => errors.push(format!("{key}: {error}")),
            }
        }

        self.disk.flush_pending_inserts();

        if !failed_dirty.is_empty() {
            match self.dirty.lock() {
                Ok(mut dirty) => dirty.extend(failed_dirty),
                Err(_) => errors.push(
                    "cache dirty-set lock poisoned while preserving failed dirty keys".to_owned(),
                ),
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/cache_memory_flush.rs"]
mod cache_memory_flush_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::ModuleContribution;

    fn minimal_result(specifier: &str) -> ImportResult {
        let mut result = ImportResult::measured(
            specifier,
            crate::ipc::protocol::MeasuredSizes {
                raw_bytes: 1,
                minified_bytes: 1,
                gzip_bytes: 1,
                brotli_bytes: 1,
                zstd_bytes: 1,
            },
        );
        result.truly_treeshakeable = true;
        result
    }

    #[test]
    fn graph_sized_entries_are_evicted_before_the_entry_count_cap() {
        let cache = ImportCache::new(None, false);
        let rows_per_entry = 100_000;
        for index in 0..6 {
            let mut result = minimal_result("pkg");
            result.internal_contributions = (0..rows_per_entry)
                .map(|row| ModuleContribution {
                    path: format!("m{row}"),
                    bytes: 1,
                })
                .collect();
            cache.insert(format!("key-{index}"), result);
        }

        let entry_weight = (rows_per_entry + 1) * GRAPH_ROW_WEIGHT_BYTES;
        assert!(cache.memory_len() * entry_weight <= MAX_MEMORY_WEIGHT_BYTES);
        assert!(cache.memory_len() < 6);
        assert!(
            cache.get_for_prewarm("key-5").is_some(),
            "the most recent entry survives"
        );
    }

    #[test]
    fn the_import_cache_refuses_an_unreusable_dependency_observation() {
        let cache = ImportCache::new(None, false);
        let key = "v4:asset-read-failed".to_owned();
        let fingerprint = crate::cache::key::unverifiable_file_fingerprint("/pkg/unreadable.woff2");

        cache.insert_with_fingerprints(key.clone(), minimal_result("asset-lib"), vec![fingerprint]);

        assert!(
            cache.memory.pin().get(&key).is_none(),
            "the store itself must reject a result whose inputs were never observed exactly"
        );
    }

    fn last_seq_of(cache: &ImportCache, key: &str) -> u64 {
        cache
            .memory
            .pin()
            .get(key)
            .map(|cached| cached.last_seq.load(Ordering::Relaxed))
            .expect("entry should be present")
    }

    fn persisted_seq_of(cache: &ImportCache, key: &str) -> u64 {
        cache
            .memory
            .pin()
            .get(key)
            .map(|cached| cached.persisted_seq.load(Ordering::Relaxed))
            .expect("entry should be present")
    }

    #[test]
    fn interactive_read_promotes_recency_bulk_read_does_not() {
        let cache = ImportCache::new(None, false);
        cache.insert("v4:react".to_owned(), minimal_result("react"));

        let seq0 = last_seq_of(&cache, "v4:react");

        // Interactive read bumps last_seq.
        assert!(cache.get_if_fresh_and_promote("v4:react").is_some());
        let seq1 = last_seq_of(&cache, "v4:react");
        assert!(
            seq1 > seq0,
            "interactive read must promote recency: {seq0} -> {seq1}"
        );

        // A second interactive read bumps it again.
        assert!(cache.get_if_fresh_and_promote("v4:react").is_some());
        let seq2 = last_seq_of(&cache, "v4:react");
        assert!(
            seq2 > seq1,
            "each interactive read promotes: {seq1} -> {seq2}"
        );

        // A bulk/prewarm read does not change last_seq.
        assert!(cache.get_for_prewarm("v4:react").is_some());
        let seq3 = last_seq_of(&cache, "v4:react");
        assert_eq!(seq3, seq2, "prewarm read must not promote recency");
    }

    #[test]
    fn swr_and_force_fresh_reads_promote_only_when_interactive() {
        let cache = ImportCache::new(None, false);
        cache.insert("v4:react".to_owned(), minimal_result("react"));

        let seq0 = last_seq_of(&cache, "v4:react");

        // Interactive stale-while-revalidate read (status-bar size) promotes recency.
        assert!(cache.get_with_result_freshness("v4:react").is_some());
        let seq1 = last_seq_of(&cache, "v4:react");
        assert!(
            seq1 > seq0,
            "interactive SWR read must promote recency: {seq0} -> {seq1}"
        );

        // Interactive force-fresh read (hover, document analysis) promotes too.
        assert!(cache.get_if_fresh_and_promote("v4:react").is_some());
        let seq2 = last_seq_of(&cache, "v4:react");
        assert!(
            seq2 > seq1,
            "interactive force-fresh read must promote recency: {seq1} -> {seq2}"
        );

        // Bulk force-fresh read (WorkspaceReport) does not promote recency.
        assert!(cache.get_if_fresh("v4:react").is_some());
        let seq3 = last_seq_of(&cache, "v4:react");
        assert_eq!(seq3, seq2, "bulk force-fresh read must not promote recency");
    }

    #[test]
    fn promoted_seq_survives_memory_cap_eviction() {
        // An entry promoted in memory but not yet flushed keeps that promotion when the
        // memory cap evicts its mirror, or the disk byte-budget evictor would see it as
        // cold.
        let dir = std::env::temp_dir().join(format!(
            "il-promote-evict-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let cache = ImportCache::new(Some(dir.clone()), true);

        // The victim: inserted first (lowest recency), so the flood below makes it the
        // memory-cap eviction target.
        let victim = "v4:victim".to_owned();
        cache.insert(victim.clone(), minimal_result("victim"));
        let born = persisted_seq_of(&cache, &victim);
        assert_eq!(
            last_seq_of(&cache, &victim),
            born,
            "a fresh insert is born with last_seq == persisted_seq"
        );

        // Promote it: an interactive read bumps last_seq above the persisted (born) seq
        // WITHOUT flushing, so disk still holds the low born seq.
        assert!(cache.get_if_fresh_and_promote(&victim).is_some());
        let promoted = last_seq_of(&cache, &victim);
        assert!(
            promoted > born,
            "the get promoted last_seq: {born} -> {promoted}"
        );
        assert_eq!(
            persisted_seq_of(&cache, &victim),
            born,
            "the promotion is not yet persisted"
        );

        // Flood past the cap. Every filler is born after the promotion, so the victim
        // stays the least-recently-used.
        for index in 0..=MAX_MEMORY_ENTRIES {
            cache.insert(format!("v4:fill-{index}"), minimal_result("fill"));
        }
        assert!(
            cache.memory.pin().get(&victim).is_none(),
            "the victim was evicted from the memory mirror"
        );

        // Re-hydrate via a non-promoting read: the disk-decoded seq is loaded verbatim
        // into last_seq, and must be the promoted seq, not the born one.
        assert!(
            cache.get_for_prewarm(&victim).is_some(),
            "the victim's disk copy survives memory-cap eviction and re-hydrates"
        );
        assert_eq!(
            last_seq_of(&cache, &victim),
            promoted,
            "the promoted seq was flushed to disk before the memory-cap eviction"
        );

        drop(cache);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn force_fresh_read_never_rides_the_ttl_fast_path() {
        // A force-fresh read (`get_if_fresh`, the `importlens check` gate) always
        // re-verifies. A node_modules change with no generation bump (a watcher-excluded
        // folder) lands inside the TTL window at the same generation, where the normal
        // fast path serves it unverified.
        let dir = std::env::temp_dir().join(format!(
            "il-rb4-force-fresh-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dep = dir.join("dep.js");
        std::fs::write(&dep, b"old").unwrap();

        // A stat-only fingerprint: mtime+len is all the check has to go on.
        let fingerprint = crate::cache::key::file_fingerprint_with_hash(&dep, None)
            .expect("fingerprint the dep file");

        // An opaque key is not first-party, so the entry is eligible for the fast path.
        let key = "v4:react".to_owned();
        let cache = ImportCache::new(None, false);
        cache.insert_with_fingerprints(key.clone(), minimal_result("react"), vec![fingerprint]);

        // Change the dep's length without bumping the cache generation.
        std::fs::write(&dep, b"new-and-longer-content").unwrap();

        // A normal read still rides the fast path and serves the now-stale value.
        assert!(
            cache.get_for_prewarm(&key).is_some(),
            "the TTL fast path is active — a normal get serves the entry without re-probing"
        );

        assert!(
            cache.get_if_fresh(&key).is_none(),
            "force-fresh must skip the TTL fast path, re-probe, and reject the stale entry (RB-4)"
        );

        drop(cache);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stylesheet's `url()` may resolve outside the package root (D18), so a workspace font can
    /// sit in a node_modules entry's fingerprint set, and it changes with no generation bump. The
    /// fingerprint below carries the real file's length and mtime with the hash of different
    /// content of the same length, which is what an mtime-preserving rewrite leaves behind.
    #[test]
    fn a_workspace_file_inside_a_node_modules_entry_is_still_hash_verified() {
        let dir = std::env::temp_dir().join(format!(
            "il-a10-strict-routing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Deliberately not under node_modules: the shape a CSS `url()` escape produces.
        let font = dir.join("Inter.woff2");
        std::fs::write(&font, b"AAAA").unwrap();
        assert!(
            !font
                .to_string_lossy()
                .replace('\\', "/")
                .contains("/node_modules/"),
            "the fixture must sit outside node_modules or it proves nothing"
        );

        let fingerprint = crate::cache::key::file_fingerprint_with_hash(
            &font,
            Some(crate::cache::key::content_hash(b"BBBB")),
        )
        .expect("fingerprint the workspace font");

        // An opaque node_modules key: `cache_key_is_first_party` reads only the entry path, so this
        // entry is "not first party" however many workspace files its fingerprints name.
        let key = "v4:ui-kit".to_owned();
        let cache = ImportCache::new(None, false);
        cache.insert_with_fingerprints(key.clone(), minimal_result("ui-kit"), vec![fingerprint]);

        assert!(
            cache.get_if_fresh(&key).is_none(),
            "the workspace font's content hash must be consulted, not skipped because the ENTRY \
             lives under node_modules"
        );

        drop(cache);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A read verifies the entry it fetched, and a recompute can replace that entry before the
    /// verdict lands. The verdict is about the old measurement only: evicting by key would throw
    /// away the fresh one from memory and its queued disk write, costing a rebuild.
    #[test]
    fn a_stale_verdict_never_evicts_the_entry_that_replaced_it() {
        let dir = std::env::temp_dir().join(format!(
            "il-evict-identity-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let cache = ImportCache::new(Some(dir.clone()), true);
        let key = "v4:react";
        let identity_of = |cache: &ImportCache| {
            Arc::clone(&cache.memory.pin().get(key).expect("entry present").last_seq)
        };

        cache.insert(key.to_owned(), minimal_result("react"));
        let verified = identity_of(&cache);
        cache.insert(key.to_owned(), minimal_result("react"));

        cache.evict_if_current(key, &verified);
        assert!(
            cache.memory.pin().get(key).is_some(),
            "the replacement must survive a verdict on the entry it replaced"
        );
        assert!(
            cache
                .disk
                .get_with_freshness(key)
                .map(|(cached, _)| cached)
                .is_some(),
            "and so must its queued disk write"
        );

        cache.evict_if_current(key, &identity_of(&cache));
        assert!(cache.memory.pin().get(key).is_none());
        assert!(
            cache
                .disk
                .get_with_freshness(key)
                .map(|(cached, _)| cached)
                .is_none()
        );

        drop(cache);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn cached_import(specifier: &str) -> CachedImport {
        CachedImport {
            result: minimal_result(specifier),
            dependency_fingerprints: Vec::new(),
            verified_generation: 0,
            verified_at: None,
            last_seq: Arc::new(AtomicU64::new(1)),
            persisted_seq: Arc::new(AtomicU64::new(1)),
            first_party: false,
        }
    }

    #[test]
    fn guarded_memory_insert_rolls_back_when_a_clear_races() {
        // A `clear()` that lands after a writer captured the clear generation but before
        // its memory insert must not leave a memory-only survivor. With disk disabled,
        // `clear()` still bumps the generation the guard keys off.
        let cache = ImportCache::new(None, false);
        let key = "v4:react".to_owned();

        // Capture the generation, THEN a clear() races in and bumps it.
        let captured = cache.disk.clear_generation();
        cache.clear();

        // The guarded insert, its captured generation superseded, must roll back.
        cache.insert_into_memory_guarded(key.clone(), cached_import("react"), captured);
        assert!(
            cache.memory.pin().get(&key).is_none(),
            "a memory insert whose captured generation a clear() superseded must roll back (RB-3)"
        );

        // Control: an insert carrying the CURRENT generation persists.
        let current = cache.disk.clear_generation();
        cache.insert_into_memory_guarded(key.clone(), cached_import("react"), current);
        assert!(
            cache.memory.pin().get(&key).is_some(),
            "a memory insert with the current generation persists"
        );
    }
}
