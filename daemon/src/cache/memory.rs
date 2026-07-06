use crate::{
    cache::{
        disk::DiskCache,
        key::{FileFingerprint, cache_key_is_orphan, cache_key_matches_any_package},
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
const REVERIFY_TTL: Duration = Duration::from_secs(30);

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
    // Runtime verification state (not persisted): the generation and monotonic
    // instant at which this entry's fingerprints were last confirmed current.
    pub verified_generation: u64,
    // `None` = never verified this run (fresh decode from disk) → the fast path
    // is skipped and the next `get` re-verifies. Monotonic so a backward
    // wall-clock jump (NTP, VM resume) cannot extend the re-verify window.
    pub verified_at: Option<Instant>,
    // Monotonic recency sequence of the last interactive hit; drives LRU
    // eviction (smallest = least-recently-used) for both the memory working set
    // and the disk byte budget. A process-global counter, not wall-clock: no
    // ties, and immune to backward clock jumps. Shared via Arc so an interactive
    // hit can bump it in place without re-inserting the entry; persisted to the
    // disk envelope as a plain `u64` at flush time.
    pub last_seq: Arc<AtomicU64>,
    // The `last_seq` value the disk layer currently knows for this entry (stamped
    // at insert/hydration, refreshed when `flush_to_disk` persists a promotion).
    // `last_seq > persisted_seq` means the entry was used since it was last
    // persisted — the eviction filter treats such entries as hot, and the flush
    // sweep re-persists them so recency survives a restart.
    pub persisted_seq: Arc<AtomicU64>,
    // Whether the cache key resolves to a first-party dependency (workspace /
    // npm link / `file:`). Key-derived and immutable for the entry's lifetime;
    // memoized here because deriving it means hex+msgpack-decoding the key, which
    // is far too expensive to repeat on every cache hit (the D3 gate consults it
    // before the TTL fast path).
    pub first_party: bool,
}

// A transient stat/read error on a dependency (`Freshness::Unknown` — a file locked
// for milliseconds by a save or an AV scan) must not immediately flash an alarming
// "couldn't verify". `get_with_result_freshness` graduates it (§4.3.1): the first few
// Unknown sightings keep serving the last value QUIETLY flagged `Stale{revalidating}`,
// and it is surfaced as `Unverified` only once the error persists past either bound.
const UNKNOWN_MAX_ATTEMPTS: u32 = 3;
const UNKNOWN_PERSIST_AFTER: Duration = Duration::from_secs(2);

/// Per-key graduation state for a transient `Unknown` freshness. Lives only while an
/// entry is mid-graduation (the slow re-check path); cleared on any non-`Unknown`
/// outcome and on re-insert, so a later blip starts a fresh window.
#[derive(Debug, Clone, Copy)]
struct UnknownRetry {
    // Monotonic — never `SystemTime` — so a backward wall-clock jump (NTP, VM resume)
    // cannot stretch or collapse the persistence window.
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
    // Per-key `Unknown` graduation windows (§4.3.1). Only keys currently on the slow
    // re-check path with an unresolved transient error appear here; entries are removed
    // on any non-`Unknown` outcome and on re-insert. Independent of the papaya `memory`
    // map — never locked while a `memory` epoch guard is held in a conflicting order.
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

/// RAII claim on an in-flight revalidation. Releasing on drop (rather than an explicit
/// `finish` call) keeps the in-flight set correct even if the recompute panics and
/// unwinds — a leaked key would otherwise block that key's revalidation until restart.
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
    /// Interactive / bulk read: serves the last-known value even on a transient
    /// `Unknown` (§4.3.1 keeps serving while the error is transient), and promotes
    /// LRU recency when `promote` is set (an interactive hit does; a prewarm scan
    /// does not — scan resistance, §5.1).
    Serve { promote: bool },
    /// Force-fresh read (§4.5 — CI / `importlens check`): serves ONLY a value
    /// verified `Fresh` against disk, across both the memory working set and the
    /// disk cache. Every non-`Fresh` state (Unknown/Stale/Gone/miss) yields `None`
    /// so the caller recomputes synchronously; an `Unknown` entry is KEPT (never
    /// deleted, never served, never hydrated). Never promotes recency.
    RequireFresh,
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
            revalidating: Mutex::new(HashSet::new()),
            unknown_retry: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Interactive read: promotes the entry's recency (bumps `last_seq`) on a hit.
    pub fn get(&self, key: &str) -> Option<ImportResult> {
        self.read(key, ReadIntent::Serve { promote: true })
    }

    /// Bulk/prewarm read: does NOT promote recency (scan resistance). A
    /// full-workspace scan or a prefetcher dedup check must not flood the
    /// recency signal and evict a user's warm working set (design §5.1).
    pub fn get_for_prewarm(&self, key: &str) -> Option<ImportResult> {
        self.read(key, ReadIntent::Serve { promote: false })
    }

    /// Force-fresh read (CI / `importlens check`): returns the cached value ONLY when
    /// it is verified `Fresh` against disk, across BOTH the in-memory working set and
    /// the disk cache. Every non-Fresh state returns `None` so the caller recomputes
    /// synchronously (§4.5 — CI never serves a stale/unverified value):
    ///   Fresh → Some(value);  Stale/Gone → evict (as the normal read does), None;
    ///   Unknown → keep (never delete), but None (do NOT serve unverified);  miss → None.
    /// Does not promote recency.
    ///
    /// This is the cold-daemon completion of the force-fresh gate: a fresh daemon
    /// hydrating a prior run's DISK cache must not serve a disk-classified `Unknown`
    /// (which the evicting `get` would launder into a `cache_hit`). Freshness is
    /// classified by the SAME plumbing the normal read uses
    /// (`check_fingerprints[_strict]`, `disk.get_with_freshness`) — only the
    /// serve-on-`Unknown` decision differs.
    pub fn get_if_fresh(&self, key: &str) -> Option<ImportResult> {
        self.read(key, ReadIntent::RequireFresh)
    }

    /// Shared read path for `get`/`get_for_prewarm` (serve semantics) and
    /// `get_if_fresh` (force-fresh semantics). Classifies the memory working set,
    /// then the disk cache; `intent` selects only recency promotion and what a
    /// transient `Unknown` does — serve the last-known value, or return `None` so a
    /// force-fresh caller recomputes. Stale/Gone always evict; a verified `Fresh`
    /// always serves (and a disk hit hydrates into memory).
    fn read(&self, key: &str, intent: ReadIntent) -> Option<ImportResult> {
        let memory = self.memory.pin();
        if let Some(cached) = memory.get(key) {
            let generation = current_cache_generation();
            // Bump LRU recency on every interactive hit. The Arc is shared with the
            // restamp clone below, so this stays current across both return paths.
            if let ReadIntent::Serve { promote: true } = intent {
                cached
                    .last_seq
                    .store(RecencyClock::next_seq(), Ordering::Relaxed);
            }
            // A force-fresh read (§4.5 — CI / `importlens check`) must ALWAYS re-verify
            // against disk: it may never ride the TTL fast path (RB-4). Otherwise a
            // node_modules change with no generation bump (a watcher-excluded folder —
            // the very case REVERIFY_TTL exists to cover, memory.rs:31-33) would be
            // served unverified inside the 30 s window, and the budget gate would judge
            // against a stale size. The fast path is a normal-read optimization only;
            // the disk-hydration path below already honors `RequireFresh`.
            //
            // First-party deps (workspace / npm link / file:) change without a
            // NodeModulesChanged generation bump, so they too must never take the TTL
            // fast path — always fall through to the tri-state re-validation below (D3).
            // `first_party` is memoized on the entry: deriving it from the key means a
            // hex+msgpack decode, far too expensive per hit.
            let fresh_without_restat = !matches!(intent, ReadIntent::RequireFresh)
                && !cached.first_party
                && cached.verified_generation == generation
                && cached
                    .verified_at
                    .is_some_and(|at| at.elapsed() < REVERIFY_TTL);

            if !fresh_without_restat {
                // D3/X-7: first-party deps are re-probed on every get, so the cheap
                // mtime+len pre-filter's blind spot (an equal-length, mtime-preserving
                // rewrite) is live on this path — hash-verify them strictly.
                // node_modules deps keep the cheap check: they change only behind a
                // NodeModulesChanged generation bump, so re-reading their bytes on
                // every hit would be pure waste.
                let freshness = if cached.first_party {
                    crate::cache::key::check_fingerprints_strict(&cached.dependency_fingerprints)
                } else {
                    crate::cache::key::check_fingerprints(&cached.dependency_fingerprints)
                };
                match freshness {
                    crate::cache::key::Freshness::Stale | crate::cache::key::Freshness::Gone => {
                        // Non-`Unknown` outcome → reset any graduation window for a key
                        // shared with the serve-stale path (§4.3.1).
                        self.clear_unknown(key);
                        memory.remove(key);
                        self.disk.remove(key);
                        return None;
                    }
                    crate::cache::key::Freshness::Unknown => {
                        // Could not verify (transient fs error). Keep the entry and do
                        // NOT restamp, so the next hit re-checks once the transient
                        // condition clears. A force-fresh read (§4.5) must NOT serve
                        // this unverified value — return `None` so the caller
                        // recomputes; a normal read serves the last-known value.
                        // (Graduation of the `Unknown` surfaced to the client is owned
                        // by `get_with_result_freshness`; this evicting read only keeps.)
                        return match intent {
                            ReadIntent::RequireFresh => None,
                            ReadIntent::Serve { .. } => {
                                let mut result = cached.result.clone();
                                result.cache_hit = true;
                                Some(result)
                            }
                        };
                    }
                    crate::cache::key::Freshness::Fresh => {
                        // Non-`Unknown` outcome → reset any graduation window for a key
                        // shared with the serve-stale path (§4.3.1).
                        self.clear_unknown(key);
                        let mut result = cached.result.clone();
                        // Restamp via `update`, which is a no-op when the key was
                        // concurrently removed (invalidation / clear / eviction) —
                        // a plain `insert` here would resurrect the removed entry.
                        // First-party entries skip it entirely: their gate above never
                        // consults the stamps, so restamping is a wasted clone.
                        if !cached.first_party {
                            memory.update(key.to_owned(), |entry| {
                                let mut restamped = entry.clone();
                                restamped.verified_generation = generation;
                                restamped.verified_at = Some(Instant::now());
                                restamped
                            });
                        }
                        result.cache_hit = true;
                        return Some(result);
                    }
                }
            }

            let mut result = cached.result.clone();
            result.cache_hit = true;
            return Some(result);
        }
        // Release the map pin before the disk probe: `get_with_freshness` stats
        // (and may read/remove) files, and holding an epoch guard across that I/O
        // delays reclamation of concurrently removed entries.
        drop(memory);

        // Capture the generation BEFORE probing disk freshness, mirroring the
        // insert and slow-path stamps. If an invalidation bumps the generation
        // during get_with_freshness, stamping the post-check (newer) generation
        // would launder a just-invalidated entry into "verified fresh" and serve
        // it on the fast path for up to REVERIFY_TTL.
        let hydration_generation = current_cache_generation();
        // Also capture the clear generation: a `clear()` racing between this disk read
        // and the memory hydration below could otherwise leave a memory-only survivor of
        // the just-cleared entry (the read twin of the insert disk-then-memory race,
        // RB-3). The guarded insert rolls it back if the generation moved.
        let clear_generation = self.disk.clear_generation();
        if let Some((mut cached, freshness)) = self.disk.get_with_freshness(key) {
            // Only stamp "verified now" when the disk layer actually confirmed
            // freshness against the file on disk. If it came back `Unknown` (a
            // transient stat/read error kept the entry instead of evicting it),
            // stamping it here would launder that transient failure into
            // "verified fresh": the next get() would take the
            // fresh_without_restat fast path and skip re-checking for up to
            // REVERIFY_TTL. Leaving the decoded defaults (generation 0 / verified_at
            // None, set by decode_cached_result) makes the very next get()
            // re-verify instead.
            if freshness == crate::cache::key::Freshness::Fresh {
                cached.verified_generation = hydration_generation;
                cached.verified_at = Some(Instant::now());
                self.clear_unknown(key);
            } else if let ReadIntent::RequireFresh = intent {
                // Disk hydration yields only Fresh or Unknown (the disk layer already
                // evicts Stale/Gone). A force-fresh read (§4.5) must not serve — or
                // even hydrate — an `Unknown`: return `None` (the disk entry is kept)
                // so the caller recomputes. A normal read falls through below and
                // hydrates+serves the last-known value.
                return None;
            }
            // §3.2: an interactive hit must promote recency even on a disk-hydration
            // hit — otherwise a just-accessed rehydrated entry keeps its old/ancient
            // persisted `last_seq` and stays a prime eviction victim on the very next
            // maintenance pass. Mirrors the memory-hit promotion above; non-promoting
            // intents (prewarm/force-fresh) keep the persisted seq unchanged.
            if let ReadIntent::Serve { promote: true } = intent {
                cached
                    .last_seq
                    .store(RecencyClock::next_seq(), Ordering::Relaxed);
            }
            let mut result = cached.result.clone();
            self.insert_into_memory_guarded(key.to_owned(), cached, clear_generation);
            result.cache_hit = true;
            // Re-hydrating from disk grows the map too, so enforce the cap here as
            // well as on fresh inserts.
            self.enforce_memory_cap();
            return Some(result);
        }

        None
    }

    /// Stale-while-revalidate read: like `get`, but NON-evicting on `Stale` — it
    /// serves the last-known value flagged `Stale { revalidating: true }` instead of
    /// dropping it, so the caller can show an instant answer and recompute in the
    /// background. `Gone` still evicts and returns `None` (recompute can't reuse a
    /// removed dep); a transient `Unknown` is GRADUATED (§4.3.1) — served quietly as
    /// `Stale { revalidating: true }` while the error is fresh and surfaced as
    /// `Unverified` only once it persists past the window; `Fresh` serves `Fresh`. Does
    /// not touch the in-flight set — dedupe is the caller's via `begin_revalidation`.
    pub fn get_with_result_freshness(&self, key: &str) -> Option<(ImportResult, ResultFreshness)> {
        self.read_with_result_freshness(key, true)
    }

    /// Shared stale-while-revalidate read path for `get_with_result_freshness`
    /// (interactive) and `get_with_result_freshness_for_bulk` (bulk). `promote`
    /// selects ONLY recency promotion: an interactive read bumps `last_seq`; a
    /// bulk/background read (WorkspaceReport, Compare) does not, so a full-workspace
    /// scan can't flood the recency signal and evict the user's warm working set
    /// (scan resistance, §5.1). Freshness handling (Fresh/Stale/Unknown, disk
    /// hydration, graduation) is otherwise identical on both paths.
    fn read_with_result_freshness(
        &self,
        key: &str,
        promote: bool,
    ) -> Option<(ImportResult, ResultFreshness)> {
        let memory = self.memory.pin();
        if let Some(cached) = memory.get(key) {
            let generation = current_cache_generation();
            // Interactive serve → promote recency; a bulk read skips the bump.
            if promote {
                cached
                    .last_seq
                    .store(RecencyClock::next_seq(), Ordering::Relaxed);
            }
            let fresh_without_restat = !cached.first_party
                && cached.verified_generation == generation
                && cached
                    .verified_at
                    .is_some_and(|at| at.elapsed() < REVERIFY_TTL);

            if !fresh_without_restat {
                // D3/X-7: first-party deps are re-probed on every get, so the cheap
                // mtime+len pre-filter's blind spot (an equal-length, mtime-preserving
                // rewrite) is live on this path — hash-verify them strictly.
                // node_modules deps keep the cheap check: they change only behind a
                // NodeModulesChanged generation bump, so re-reading their bytes on
                // every hit would be pure waste.
                let freshness = if cached.first_party {
                    crate::cache::key::check_fingerprints_strict(&cached.dependency_fingerprints)
                } else {
                    crate::cache::key::check_fingerprints(&cached.dependency_fingerprints)
                };
                match freshness {
                    crate::cache::key::Freshness::Gone => {
                        // Non-`Unknown` outcome → reset the graduation window (§4.3.1).
                        self.clear_unknown(key);
                        memory.remove(key);
                        self.disk.remove(key);
                        return None;
                    }
                    crate::cache::key::Freshness::Stale => {
                        // Non-`Unknown` outcome → reset the graduation window (§4.3.1).
                        self.clear_unknown(key);
                        // Serve-stale: keep the entry (do NOT restamp — it must stay on
                        // the slow path so later gets keep re-checking until a
                        // background recompute replaces it) and flag it stale.
                        let mut result = cached.result.clone();
                        result.cache_hit = true;
                        result.freshness = ResultFreshness::stale(true);
                        return Some((result, ResultFreshness::stale(true)));
                    }
                    crate::cache::key::Freshness::Unknown => {
                        // Graduate the transient error (§4.3.1): quietly
                        // `Stale{revalidating}` while it is fresh, `Unverified` only once
                        // it persists past the window. Do NOT restamp (stay on the slow
                        // path so later gets keep re-checking) and do NOT delete — keep
                        // serving the last-known value throughout.
                        let mut result = cached.result.clone();
                        result.cache_hit = true;
                        let freshness = self.record_unknown(key);
                        result.freshness = freshness.clone();
                        return Some((result, freshness));
                    }
                    crate::cache::key::Freshness::Fresh => {
                        // Non-`Unknown` outcome → reset the graduation window (§4.3.1).
                        self.clear_unknown(key);
                        let mut result = cached.result.clone();
                        // Restamp via `update` — a no-op when the key was concurrently
                        // removed, so a racing invalidation/clear is never resurrected.
                        // First-party entries skip it: their gate never reads the stamps.
                        if !cached.first_party {
                            memory.update(key.to_owned(), |entry| {
                                let mut restamped = entry.clone();
                                restamped.verified_generation = generation;
                                restamped.verified_at = Some(Instant::now());
                                restamped
                            });
                        }
                        result.cache_hit = true;
                        result.freshness = ResultFreshness::fresh();
                        return Some((result, ResultFreshness::fresh()));
                    }
                }
            }

            let mut result = cached.result.clone();
            result.cache_hit = true;
            result.freshness = ResultFreshness::fresh();
            return Some((result, ResultFreshness::fresh()));
        }
        drop(memory);

        // Disk-hydration path: `disk.get_with_freshness` already evicts Stale/Gone, so
        // it only yields Fresh (served Fresh) or Unknown (kept, graduated per §4.3.1).
        // A disk-only stale entry is therefore not served stale here — it falls through
        // to recompute, which is acceptable (the common serve-stale case is the memory
        // working set above).
        let hydration_generation = current_cache_generation();
        // Capture the clear generation before the disk read so a racing `clear()` can't
        // leave a memory-only survivor of the hydrated entry (RB-3, as in `read`).
        let clear_generation = self.disk.clear_generation();
        if let Some((mut cached, freshness)) = self.disk.get_with_freshness(key) {
            let result_freshness = if freshness == crate::cache::key::Freshness::Fresh {
                cached.verified_generation = hydration_generation;
                cached.verified_at = Some(Instant::now());
                self.clear_unknown(key);
                ResultFreshness::fresh()
            } else {
                // Disk hydration yields only Fresh or Unknown (the disk layer already
                // evicts Stale/Gone), so this branch is an `Unknown` — graduate it too
                // (§4.3.1) rather than flashing Unverified on a cold, transiently-locked
                // hydrate. The entry is inserted below, so subsequent gets continue the
                // window through the memory path above.
                self.record_unknown(key)
            };
            // §3.2: promote recency on an interactive disk-hydration hit, mirroring
            // the memory-hit promotion above — otherwise a just-accessed rehydrated
            // entry keeps its persisted `last_seq` and stays a prime eviction victim
            // on the next maintenance pass. A bulk read keeps the persisted seq.
            if promote {
                cached
                    .last_seq
                    .store(RecencyClock::next_seq(), Ordering::Relaxed);
            }
            let mut result = cached.result.clone();
            result.freshness = result_freshness.clone();
            self.insert_into_memory_guarded(key.to_owned(), cached, clear_generation);
            result.cache_hit = true;
            self.enforce_memory_cap();
            return Some((result, result_freshness));
        }

        None
    }

    /// Bulk/background stale-while-revalidate read (WorkspaceReport, Compare): serves
    /// the SAME freshness info as `get_with_result_freshness` but does NOT promote
    /// recency (scan resistance, §5.1), so a full-workspace scan can't flood the
    /// recency signal and evict the user's warm working set.
    pub fn get_with_result_freshness_for_bulk(
        &self,
        key: &str,
    ) -> Option<(ImportResult, ResultFreshness)> {
        self.read_with_result_freshness(key, false)
    }

    /// Claim ownership of a background revalidation. Returns `Some(guard)` for the
    /// first caller (which should spawn the recompute) and `None` while that claim is
    /// already in flight. The guard releases the claim on drop — including on
    /// panic/unwind — so a recompute that panics cannot leak the claim forever.
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

    /// Release an in-flight claim. Prefer the `RevalidationGuard` returned by
    /// `begin_revalidation`, which calls this on drop (panic-safe); this is the
    /// guard's release primitive.
    fn finish_revalidation(&self, claim_key: &str) {
        let mut inflight = self
            .revalidating
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inflight.remove(claim_key);
    }

    /// Records another transient `Unknown` sighting for `key` and returns the serve-time
    /// freshness it graduates to (§4.3.1). While the error is fresh — within
    /// `UNKNOWN_MAX_ATTEMPTS` sightings AND `UNKNOWN_PERSIST_AFTER` of monotonic time —
    /// the last value is served QUIETLY as `Stale { revalidating: true }` (a quiet
    /// recheck); once it persists past either bound it is surfaced as
    /// `Unverified { reason }`. Never deletes and never claims `Fresh`.
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
    /// outcome (Fresh/Stale/Gone) and on re-insert, so a later transient error starts a
    /// fresh window rather than inheriting a stale `first_seen`/`attempts` that would
    /// immediately surface `Unverified`.
    fn clear_unknown(&self, key: &str) {
        let mut retries = self
            .unknown_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retries.remove(key);
    }

    /// Re-probe the raw dependency freshness of a *memory-resident* `key` WITHOUT
    /// serving or restamping it. The background SWR revalidation uses this to tell a
    /// genuine content-`Stale` entry (which should recompute) from one graduated to
    /// `Stale` by a transient `Unknown` (§4.3.1): the latter must NEVER be routed into
    /// recompute — re-analyzing would re-hit the same stat/read error and could
    /// overwrite the good cached value with an error result. Re-stats every dependency
    /// (bypassing the TTL fast path), so the call itself is the active re-check for a
    /// graduated key. Returns `None` when the key is not in the memory working set.
    pub fn probe_freshness(&self, key: &str) -> Option<crate::cache::key::Freshness> {
        let memory = self.memory.pin();
        let cached = memory.get(key)?;
        let freshness = if cached.first_party {
            crate::cache::key::check_fingerprints_strict(&cached.dependency_fingerprints)
        } else {
            crate::cache::key::check_fingerprints(&cached.dependency_fingerprints)
        };
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
    pub fn insert_with_fingerprints_at_generation(
        &self,
        key: String,
        result: ImportResult,
        dependency_fingerprints: Vec<FileFingerprint>,
        verified_generation: u64,
    ) {
        // A freshly computed value is the definitive reset point for this key's
        // `Unknown` graduation window (§4.3.1): clearing here means a later transient
        // error starts a NEW window instead of inheriting a stale `first_seen`/`attempts`
        // from a prior episode (which would wrongly surface `Unverified` immediately).
        self.clear_unknown(&key);
        // Capture the clear generation BEFORE the disk enqueue and memory insert, and
        // tag both with it, so a `clear()` racing this insert supersedes the disk copy
        // (dropped at flush) and the memory-guard rolls back the memory copy — the two
        // never diverge (RB-3).
        let clear_generation = self.disk.clear_generation();
        let born_seq = RecencyClock::next_seq();
        let cached = CachedImport {
            result,
            dependency_fingerprints,
            verified_generation,
            verified_at: Some(Instant::now()),
            last_seq: Arc::new(AtomicU64::new(born_seq)),
            // The disk insert below persists this same seq (queued for the
            // batched flush), so the entry is born with nothing to re-persist.
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

    /// Evicts the least-recently-used entries while the in-memory map is over the
    /// cap. The disk copy (if any) survives and re-hydrates on the next hit, so
    /// this only sheds the memory mirror. Called from every path that grows the
    /// map (fresh insert and disk re-hydration), not the restamp path (which
    /// replaces an existing key and cannot grow the map).
    ///
    /// Evicts in one batch down to ~90% of the cap: a session pinned at the cap
    /// then pays one sort per ~400 inserts instead of a full min-scan per insert.
    /// `dirty` entries (whose disk insert failed) are never evicted — they exist
    /// only in memory, and dropping one would silently lose the computed result
    /// before `flush_to_disk` can replay it.
    fn enforce_memory_cap(&self) {
        let memory = self.memory.pin();
        if memory.len() <= MAX_MEMORY_ENTRIES {
            return;
        }

        // Capture the clear generation before the candidate snapshot: the re-persist
        // below reads a memory entry that may be wiped by a racing `clear()`, so tag its
        // disk write with this generation — a clear that lands mid-eviction bumps the
        // generation and the flush filter drops the write instead of resurrecting the
        // shard (RB-3).
        let clear_generation = self.disk.clear_generation();
        let dirty = self
            .dirty
            .lock()
            .map(|dirty| dirty.clone())
            .unwrap_or_default();
        let mut candidates = memory
            .iter()
            .filter(|(key, _)| !dirty.contains(*key))
            .map(|(key, entry)| (entry.last_seq.load(Ordering::Relaxed), key.clone()))
            .collect::<Vec<_>>();
        candidates.sort_unstable();

        let target = MAX_MEMORY_ENTRIES * 9 / 10;
        let excess = memory.len().saturating_sub(target);
        for (_, key) in candidates.into_iter().take(excess) {
            // Before dropping the memory mirror, persist any UNFLUSHED recency
            // promotion (an interactive hit bumps `last_seq` past `persisted_seq`;
            // the sweep in `flush_to_disk` normally re-persists it). The disk copy
            // survives this eviction and re-hydrates later, but with the STALE low
            // persisted seq — so a just-used entry would look cold to the byte-budget
            // evictor and become a prime disk victim. Flush the promoted seq here so
            // its recency survives; once the entry leaves the working set,
            // `flush_to_disk`'s sweep can no longer reach it (F6).
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
        // disk.clear() bumps the clear generation BEFORE it wipes; the memory-insert
        // paths captured that generation before their insert and roll back if it moved,
        // so an insert racing this clear cannot leave a memory-only survivor (RB-3).
        self.disk.clear();
        self.memory.pin().clear();
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.clear();
        }
        if let Ok(mut retries) = self.unknown_retry.lock() {
            retries.clear();
        }
    }

    /// Inserts into the memory mirror, guarding against a racing `clear()`. `clear()`
    /// wipes disk+memory and bumps the disk clear generation; a change from
    /// `captured_generation` (read before the caller started deriving `cached`) means a
    /// wipe may have run between that capture and this insert, which would otherwise
    /// leave a memory-only survivor of a cleared cache — the "Clear actually clears"
    /// honesty bug (RB-3). The caller passes the SAME generation it tagged the paired
    /// `disk.insert_at_generation` with, so the disk and memory copies live or die
    /// together. Does not enforce the memory cap — callers that grow the map do that
    /// after.
    fn insert_into_memory_guarded(
        &self,
        key: String,
        cached: CachedImport,
        captured_generation: u64,
    ) {
        // Pre-check: if a clear() already superseded our generation, skip the insert
        // entirely. A stale insert would need rolling back and, worse, could clobber a
        // concurrent post-clear insert of the same key — skipping avoids both.
        if self.disk.clear_generation() != captured_generation {
            return;
        }
        // Identity of the value we insert (its unique last_seq Arc), so the rollback
        // removes ONLY our own entry — never a concurrent, legitimately post-clear
        // insert of the same key that replaced ours between our insert and the re-check.
        let our_last_seq = Arc::clone(&cached.last_seq);
        let memory = self.memory.pin();
        memory.insert(key.clone(), cached);
        // Re-check: a clear() landing during/after our insert (its wipe may have preceded
        // our insert) must still roll us back — but by identity, leaving a racing fresh
        // insert intact.
        if self.disk.clear_generation() != captured_generation {
            memory.compute(key, |entry| match entry {
                Some((_, current)) if Arc::ptr_eq(&current.last_seq, &our_last_seq) => {
                    papaya::Operation::Remove
                }
                _ => papaya::Operation::Abort(()),
            });
        }
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

    /// One-pass byte/recency summary of this cache's disk shard for the capacity
    /// coordinator. Empty when the disk cache is disabled.
    pub fn shard_rollup(&self) -> crate::cache::disk::ShardRollup {
        self.disk.shard_rollup()
    }

    /// The largest persisted recency seq in this shard's disk layer — a single-key
    /// SUMMARY read for the startup recency seed (C5). `0` when the disk cache is
    /// disabled. See `DiskCache::summary_max_seq`.
    pub fn summary_max_seq(&self) -> u64 {
        self.disk.summary_max_seq()
    }

    /// Up to `n` genuinely-cold eviction victims: the shard's lowest-persisted-seq
    /// keys beyond its `floor` newest (per-project floor), SKIPPING any that are
    /// memory-hot. Used by the byte-budget evictor.
    ///
    /// An entry is memory-hot when it is resident with an in-memory `last_seq`
    /// promoted past the persisted seq the disk index sorted it by: an interactive
    /// hit bumps only `last_seq` (the persisted seq refreshes at `flush_to_disk`),
    /// so a hot entry's true recency is higher than the index knows. It was used
    /// since it was last persisted and is never a correct victim.
    ///
    /// Because a hot entry keeps its low persisted seq in the index, the lowest-`n`
    /// batch can be entirely hot even though thousands of genuinely-cold entries
    /// sit deeper in the shard. Rather than give up and let the evictor retire a
    /// shard that is still far over budget (Finding 10c), this PAGES past the hot
    /// keys through the ascending index until it collects `n` cold keys or exhausts
    /// the evictable region — bounded by `MAX_EVICTION_SCAN` so a shard whose whole
    /// evictable prefix is hot still returns empty (→ the evictor retires it) in
    /// O(log N + window) rather than scanning unbounded.
    pub fn lowest_seq_disk_keys(&self, n: usize, floor: u64) -> Vec<String> {
        if n == 0 {
            return Vec::new();
        }

        // Fast path: the `n` lowest-persisted-seq keys. When none are memory-hot
        // (the common case) this is exactly the old single batch — no wider scan.
        let first = self.disk.lowest_seq_keys(n, floor);
        let region_exhausted = first.len() < n;
        let cold = self.filter_evictable(first, n);
        if cold.len() == n || region_exhausted {
            // Batch filled, or the evictable region (everything past the floor)
            // held fewer than `n` keys total, so there is nothing deeper to page
            // to — the shortfall, if any, is real.
            return cold;
        }

        // The lowest `n` under-filled because ≥1 was memory-hot AND the evictable
        // region extends past them, so cold victims may sit deeper. Page a bounded
        // wider window, skipping the hot keys, to find them instead of retiring a
        // shard that still has evictable entries. `MAX_EVICTION_SCAN` caps the
        // scan: a shard whose entire evictable prefix is hot returns fewer than
        // `n` (or empty), and the evictor's progress guard retires it for the pass.
        // Filling a full batch of `n` relies on `n <= MAX_EVICTION_SCAN`; the sole
        // caller passes `n = EVICTION_BATCH` and the window is `8 * EVICTION_BATCH`.
        let wide = self
            .disk
            .lowest_seq_keys(crate::cache::budget::MAX_EVICTION_SCAN, floor);
        self.filter_evictable(wide, n)
    }

    /// Collects up to `n` NOT-memory-hot keys from ascending `(key, persisted_seq)`
    /// candidates, preserving their least-recently-used-first order. A key is
    /// memory-hot when it is resident in the working set with `last_seq` promoted
    /// strictly past the persisted seq the index sorted it by (used since its last
    /// persist) — never a correct eviction victim.
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
    /// the on-disk bytes freed. Budget enforcement is the disk deletion; dropping
    /// the memory mirror just keeps the working set consistent with disk.
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
    // drain that queue. Any entry whose enqueue failed (serialization error) is
    // marked dirty and re-enqueued here before the queue is flushed.
    pub fn flush_to_disk(&self) -> Result<(), String> {
        // Capture the clear generation BEFORE snapshotting memory: both loops below
        // re-persist entries read from that snapshot, so tag their disk writes with this
        // generation. A `clear()` landing between the snapshot and the enqueue bumps the
        // generation, and the flush filter then drops these now-stale writes instead of
        // resurrecting the wiped shard (RB-3).
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

        // Recency sweep: interactive hits bump only the in-memory `last_seq`;
        // re-persist every entry promoted since its last persist so session
        // recency survives a restart (the cross-restart half of LRU fidelity —
        // within a session, `lowest_seq_disk_keys` shields hot entries).
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
            // Capture the seq BEFORE the insert: a concurrent promotion landing
            // mid-flush must leave `persisted_seq` at or behind what disk holds
            // (behind is safe — it just re-persists next flush; ahead would hide
            // the promotion from future sweeps).
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

    fn minimal_result(specifier: &str) -> ImportResult {
        ImportResult {
            specifier: specifier.to_owned(),
            raw_bytes: 1,
            minified_bytes: 1,
            gzip_bytes: 1,
            brotli_bytes: 1,
            zstd_bytes: 1,
            cache_hit: false,
            side_effects: false,
            truly_treeshakeable: true,
            is_cjs: false,
            confidence: Default::default(),
            confidence_reasons: Vec::new(),
            error: None,
            diagnostics: Vec::new(),
            module_breakdown: None,
            shared_bytes: None,
            freshness: ResultFreshness::fresh(),
            internal_contributions: Vec::new(),
        }
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
    fn interactive_get_promotes_recency_bulk_read_does_not() {
        let cache = ImportCache::new(None, false);
        cache.insert("v4:react".to_owned(), minimal_result("react"));

        let seq0 = last_seq_of(&cache, "v4:react");

        // Interactive get bumps last_seq.
        assert!(cache.get("v4:react").is_some());
        let seq1 = last_seq_of(&cache, "v4:react");
        assert!(
            seq1 > seq0,
            "interactive get must promote recency: {seq0} -> {seq1}"
        );

        // A second interactive get bumps it again.
        assert!(cache.get("v4:react").is_some());
        let seq2 = last_seq_of(&cache, "v4:react");
        assert!(
            seq2 > seq1,
            "each interactive get promotes: {seq1} -> {seq2}"
        );

        // A bulk/prewarm read must NOT change last_seq (scan resistance).
        assert!(cache.get_for_prewarm("v4:react").is_some());
        let seq3 = last_seq_of(&cache, "v4:react");
        assert_eq!(seq3, seq2, "prewarm read must not promote recency");
    }

    #[test]
    fn swr_result_freshness_read_promotes_only_when_interactive() {
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

        // Bulk SWR read (WorkspaceReport / Compare) must NOT promote recency
        // (scan resistance, §5.1) — a full-workspace scan can't flood the recency
        // signal and evict the user's warm working set.
        assert!(
            cache
                .get_with_result_freshness_for_bulk("v4:react")
                .is_some()
        );
        let seq2 = last_seq_of(&cache, "v4:react");
        assert_eq!(seq2, seq1, "bulk SWR read must not promote recency");
    }

    #[test]
    fn promoted_seq_survives_memory_cap_eviction() {
        // F6: an entry promoted in memory (last_seq bumped past persisted_seq by an
        // interactive hit) but not yet flushed must not lose that promotion when the
        // memory cap evicts its mirror — otherwise disk keeps the stale low seq and the
        // just-used entry reads as cold to the disk byte-budget evictor. enforce_memory_cap
        // now flushes the promoted seq before dropping the mirror.
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

        // Promote it: an interactive get bumps last_seq above the persisted (born) seq
        // WITHOUT flushing, so disk still holds the low born seq.
        assert!(cache.get(&victim).is_some());
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

        // Flood past the cap so the victim (lowest last_seq) is memory-cap evicted.
        // Every filler is inserted AFTER the promotion, so its born seq exceeds the
        // victim's promoted seq — the victim stays the least-recently-used.
        for index in 0..=MAX_MEMORY_ENTRIES {
            cache.insert(format!("v4:fill-{index}"), minimal_result("fill"));
        }
        assert!(
            cache.memory.pin().get(&victim).is_none(),
            "the victim was evicted from the memory mirror"
        );

        // Re-hydrate via a NON-promoting read: the disk-decoded seq is loaded verbatim
        // into last_seq. With the fix it is the promoted seq (flushed at eviction); a
        // regression would show the stale born seq.
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
        // RB-4: a force-fresh read (`get_if_fresh` — the CI / `importlens check` budget
        // gate, §4.5) must ALWAYS re-verify against disk. A node_modules change with no
        // generation bump (a watcher-excluded folder — the very case REVERIFY_TTL exists
        // for) lands inside the 30 s TTL window at the same generation, so the normal
        // fast path would serve it unverified. Force-fresh must skip that fast path and
        // catch the staleness. Regression: before the fix `get_if_fresh` shared the
        // `fresh_without_restat` gate and returned the stale value here too.
        let dir = std::env::temp_dir().join(format!(
            "il-rb4-force-fresh-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dep = dir.join("dep.js");
        std::fs::write(&dep, b"old").unwrap();

        // A stat-only fingerprint (content_hash: None) — the node_modules cheap-check
        // shape, where mtime+len is all `check_fingerprints` has to go on.
        let fingerprint = crate::cache::key::file_fingerprint_with_hash(&dep, None)
            .expect("fingerprint the dep file");

        // A non-first-party (opaque) key, so the entry is eligible for the TTL fast path
        // (first-party keys bypass it unconditionally).
        let key = "v4:react".to_owned();
        let cache = ImportCache::new(None, false);
        cache.insert_with_fingerprints(key.clone(), minimal_result("react"), vec![fingerprint]);

        // Change the dep so it is genuinely stale on disk (different length → mtime+len
        // mismatch → Stale) WITHOUT bumping the cache generation — exactly the
        // watcher-excluded node_modules case.
        std::fs::write(&dep, b"new-and-longer-content").unwrap();

        // A normal interactive read still rides the fast path (verified_at < TTL, same
        // generation) and serves the now-stale value: the fast path is genuinely live.
        assert!(
            cache.get(&key).is_some(),
            "the TTL fast path is active — a normal get serves the entry without re-probing"
        );

        // The force-fresh read MUST re-verify, detect the staleness, evict, and return
        // None so the caller recomputes synchronously.
        assert!(
            cache.get_if_fresh(&key).is_none(),
            "force-fresh must skip the TTL fast path, re-probe, and reject the stale entry (RB-4)"
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
        // RB-3 (memory side): the insert and disk-hydration paths are disk-then-memory.
        // If a `clear()` wipes the map AFTER a writer captured the clear generation but
        // BEFORE its memory insert lands, the entry would survive in memory only — the
        // "Clear cache silently doesn't clear" trust failure. The guard rolls it back
        // when the generation moved. Memory-only cache (disk disabled): `clear()` still
        // bumps the generation, which is exactly what the guard keys off.
        let cache = ImportCache::new(None, false);
        let key = "v4:react".to_owned();

        // Capture the generation, THEN a clear() races in and bumps it.
        let captured = cache.disk.clear_generation();
        cache.clear();

        // The guarded insert — its captured generation now superseded — must roll back.
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
