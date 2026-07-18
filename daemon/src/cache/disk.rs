use crate::{
    cache::key::{
        ANALYZER_VERSION, FileFingerprint, cache_key_is_orphan, cache_key_matches_any_package,
    },
    cache::memory::CachedImport,
    ipc::protocol::{ImportResult, ModuleContribution},
};
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    WriteTransaction,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, RwLockReadGuard, atomic::AtomicU64},
    time::Duration,
};

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";

// O(1) per-shard rollup — the incrementally-maintained byte/count/recency
// totals, so `shard_rollup` reads three scalars instead of scanning CACHE_TABLE.
const SUMMARY_TABLE: TableDefinition<&str, u64> = TableDefinition::new("summary");
const SUMMARY_TOTAL_BYTES: &str = "total_bytes";
const SUMMARY_ENTRY_COUNT: &str = "entry_count";
// Recency high-water: the largest `last_seq` ever inserted. Advances on insert,
// is left untouched by removals (a high-water mark), and is recomputed by a full
// scan in `rebuild_summary_from_scan`. Used to keep the recency clock ahead of
// persisted seqs in O(1) (and by C5's startup seed).
const SUMMARY_MAX_SEQ: &str = "max_seq";
// Secondary index: ascending `(last_seq, key)` → the evictor's lowest-N is a
// bounded range read instead of a full sort-scan, and `oldest_seq` is the first
// key. redb 4.1 supports the tuple key `(u64, &str)` with a `()` value natively,
// and its `Key` impl compares `u64` numerically (then the key lexicographically),
// so ascending iteration is exactly ascending-by-seq order.
const SEQ_INDEX_TABLE: TableDefinition<(u64, &str), ()> = TableDefinition::new("seq_index");

// v8: `ImportResult`'s five size fields became `Option<u64>` (ADR-0006). A v7 row does NOT fail
// to decode into the new struct — msgpack is happy to read the old `17550` as `Some(17550)` — and
// that is exactly the danger: every fabricated size a v7 daemon wrote (a manifest fallback's
// on-disk directory bytes, a timed-out build's entry file measured alone) would come back as a
// GENUINE measurement, indistinguishable from one, with nothing left in the record to say it was
// invented. The wipe is total, so it is sufficient: no fabricated size survives the upgrade.
// v7: adds SUMMARY_TABLE (O(1) rollup) and SEQ_INDEX_TABLE (by-`last_seq`
// secondary index), both maintained in the SAME write transaction as every
// CACHE_TABLE mutation so a crash can't tear accounting from data.
// v6: the stored value gained the fixed 8-byte `last_seq` prefix (see
// `SEQ_PREFIX_LEN`), which shifts the msgpack envelope — older rows would
// misparse, so bumping the schema wipes them on the first upgraded open via the
// existing recreate-on-mismatch path (v5 did the same for the identity-v4 key
// change). The retired `cache_recents` table from pre-v5 builds is never
// opened; it is harmless dead space reclaimed by the compactor.
const CURRENT_SCHEMA_VERSION: u64 = 8;
const INSERT_FLUSH_BATCH: usize = 64;
/// Compact a shard when more than this fraction of its `.redb` file is
/// reclaimable free space (redb reuses freed pages rather than shrinking).
pub const COMPACT_THRESHOLD: f64 = 0.5;
/// A shard is compaction-eligible only after it has stayed idle — no get or
/// insert — for at least this long (§5.5 / Finding 12). `Database::compact`
/// holds the exclusive lock across the whole rewrite, so compacting a shard the
/// user is actively analyzing would stall their concurrent gets. Measured
/// against the coarse `last_access` millis clock stamped on the hot paths.
const COMPACT_IDLE: Duration = Duration::from_secs(5);

// Every CACHE_TABLE value is `[last_seq: u64 LE, 8 bytes][msgpack CacheEnvelope]`.
// The recency readers that still scan CACHE_TABLE (`recent_keys`, and the
// summary rebuild/heal) only need `last_seq` + the value length; the fixed prefix
// lets them read it without deserializing the full envelope (ImportResult +
// contributions + fingerprints — KBs and dozens of allocations per entry).
// `shard_rollup`/`lowest_seq_keys` no longer scan at all — they read the summary
// and the `(last_seq, key)` index directly.
const SEQ_PREFIX_LEN: usize = 8;

#[cfg(test)]
#[path = "../../tests/unit/cache_disk_test_support.rs"]
pub(crate) mod test_support;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    analyzer_version: String,
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
    full_contributions: Vec<ModuleContribution>,
}

/// A shard's contribution to the global byte budget: its total on-disk bytes, the
/// oldest recency sequence it holds (the victim-selection key), and its entry
/// count. Read O(1) by `DiskCache::shard_rollup` from the incrementally-maintained
/// SUMMARY table plus the first key of the `(last_seq, key)` index — no full scan.
/// The evictor still re-reads its victim after each round, but that read is now
/// three scalars and an index descent rather than a table scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardRollup {
    pub total_bytes: u64,
    /// The smallest `last_seq` across the shard's entries. `u64::MAX` for an empty
    /// shard, so the global evictor (which targets the shard with the *smallest*
    /// `oldest_seq`) never selects a shard with nothing to evict.
    pub oldest_seq: u64,
    pub entry_count: u64,
}

impl ShardRollup {
    pub fn empty() -> Self {
        Self {
            total_bytes: 0,
            oldest_seq: u64::MAX,
            entry_count: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct DiskCache {
    // Behind an RwLock so the compactor can take exclusive `&mut Database`
    // (`Database::compact` requires it) while every normal read/write shares the
    // read lock — redb already serializes its own writers, so shared access is
    // enough for them and preserves concurrent readers. The exclusive write lock
    // additionally guarantees no live read transaction during compaction.
    db: RwLock<Option<Database>>,
    // Serialized envelopes awaiting a batched commit; drained at a size
    // threshold, on recent_keys, on recycle (flush_to_disk), and on Drop. The
    // value is `(clear_generation at enqueue, the exact bytes written to
    // CACHE_TABLE)`. The generation tag lets a flush drop an entry that a `clear()`
    // superseded after it was queued (RB-3): `clear()` bumps `clear_generation`, so
    // any entry still carrying the pre-bump generation is stale and never written.
    pending_inserts: Mutex<HashMap<String, (u64, Vec<u8>)>>,
    // Bumped by `clear()`. A writer captures it before deriving its bytes and tags the
    // queued entry with it; `flush_pending_inserts` writes only entries whose tag still
    // equals the current value, so a wipe that lands mid-flush can't resurrect a
    // pre-clear entry (RB-3). Paired with `clear_lock` so the bump+wipe and the
    // read+write are mutually exclusive — no interleave can slip a stale entry through.
    clear_generation: AtomicU64,
    // Serializes `clear()`'s (bump generation + wipe tables + drop pending) against
    // `flush_pending_inserts`'s (read generation + write kept entries), so the two
    // never interleave. Off the per-insert hot path: only the batched flush and the
    // rare clear take it.
    clear_lock: Mutex<()>,
    // Coarse wall-clock millis of the last get/insert on this shard, stamped
    // (relaxed) on those hot paths. The compaction idle gate reads it so a shard
    // the user is actively analyzing is never compacted (§5.5 / Finding 12). A
    // heuristic, not a correctness gate — a relaxed store/load is enough.
    last_access: AtomicU64,
}

impl DiskCache {
    pub fn new(storage_path: Option<PathBuf>, enabled: bool) -> Self {
        if !enabled {
            return Self::disabled();
        }

        let storage_path = match storage_path {
            Some(path) => path,
            None => return Self::disabled(),
        };

        Self {
            db: RwLock::new(Self::open_database(&storage_path)),
            pending_inserts: Mutex::new(HashMap::new()),
            clear_generation: AtomicU64::new(0),
            clear_lock: Mutex::new(()),
            // Seed to the epoch (idle), NOT `now`: a maintenance pass temp-opens
            // every unloaded shard fresh (`collect_shard_targets`) and compacts it
            // a few ms later in the SAME pass, so a `now` seed would make every
            // cold, heavily-evicted shard read as "recently accessed" and never
            // compact — the exact case compaction exists for. A real get/insert
            // stamps `now` and protects an actively-analyzed shard; the brief
            // open->first-access window reading as idle is benign.
            last_access: AtomicU64::new(0),
        }
    }

    /// Acquires the shared read lock and returns the guard when a database is
    /// open. Every normal operation borrows `&Database` through this; the guard
    /// must be held for the lifetime of any redb transaction opened from it.
    fn db_read(&self) -> Option<RwLockReadGuard<'_, Option<Database>>> {
        let guard = self.db.read().unwrap_or_else(|poison| poison.into_inner());
        guard.is_some().then_some(guard)
    }

    /// Stamps the shard's last-access clock so the compaction idle gate can tell
    /// a shard the user is actively analyzing from one that has gone quiet.
    /// Called on the `get`/`insert` hot paths — a single relaxed store of coarse
    /// wall-clock millis, off the critical path (§5.5 / Finding 12).
    fn stamp_access(&self) {
        self.last_access.store(
            crate::time::unix_millis_now(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Test-only seam: pushes the last-access clock to the epoch so the shard
    /// reads as idle to `compact_if_fragmented`, letting a test exercise the
    /// idle gate without sleeping `COMPACT_IDLE`. `#[doc(hidden)]` and not part
    /// of the supported API — the idle window is wall-clock based, so there is
    /// otherwise no deterministic way to make a just-written shard read as idle.
    #[doc(hidden)]
    pub fn mark_idle_for_test(&self) {
        self.last_access
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a database is actually open. False when disk caching is disabled
    /// OR the open failed (e.g. `DatabaseAlreadyOpen` while a maintenance pass
    /// temporarily holds the file) — callers that intended persistence can use
    /// this to avoid committing to a silently disabled cache.
    pub fn is_available(&self) -> bool {
        self.db_read().is_some()
    }

    pub fn get(&self, key: &str) -> Option<CachedImport> {
        self.get_entry(key).map(|(cached, _)| cached)
    }

    /// Like `get`, but also reports the `Freshness` classification the entry was
    /// served under (`Fresh` or `Unknown` — `Stale`/`Gone` evict inside
    /// `get_entry` and never reach a caller). Callers that mirror the entry into
    /// another layer (the in-memory cache re-hydrating from a disk hit) need
    /// this so they don't stamp an `Unknown`-survived entry as freshly verified.
    pub fn get_with_freshness(
        &self,
        key: &str,
    ) -> Option<(CachedImport, crate::cache::key::Freshness)> {
        self.get_entry(key)
    }

    pub fn load_recent(&self, limit: usize) -> Vec<(String, CachedImport)> {
        if limit == 0 {
            return Vec::new();
        }

        self.recent_keys(limit)
            .into_iter()
            .filter_map(|key| self.get_entry(&key).map(|(cached, _)| (key, cached)))
            .collect()
    }

    fn get_entry(&self, key: &str) -> Option<(CachedImport, crate::cache::key::Freshness)> {
        // Read-your-writes: a queued insert not yet flushed is not in the table.
        if let Some(entry) = self.pending_insert_entry(key) {
            // A pending hit is a real access on an enabled shard — stamp it.
            self.stamp_access();
            return Some(entry);
        }

        // Scope the read guard: `remove` re-acquires the db lock, and a re-entrant
        // read while a compaction writer is queued deadlocks (std `RwLock` blocks
        // new readers behind a queued writer, and its docs say a re-entrant `read`
        // may deadlock). Decide inside the scope, drop the guard, THEN remove.
        let decoded = {
            let db_guard = self.db_read()?;
            // Stamp only once the DB is confirmed open (after the disabled
            // short-circuit), so a no-op cache skips the clock syscall. Before the
            // table read, so a get MISS still counts as shard activity.
            self.stamp_access();
            let db = db_guard.as_ref().expect("db present under read guard");
            let read_txn = db.begin_read().ok()?;
            let table = read_txn.open_table(CACHE_TABLE).ok()?;
            let value = table.get(key).ok()??;
            decode_cached_result(value.value())
        };

        let Some(mut cached) = decoded else {
            // Undecodable row (corrupt or written by an incompatible build).
            self.remove(key);
            return None;
        };
        // **The durability gate is on the READ too, not only on `insert_at_generation`**
        // (ADR-0006, invariant 3). A write-side gate protects a store from what it is handed
        // today; it does nothing about what is already on disk. L2 outlives the process, so a row
        // written by a build that predates the gate — or by any future path that reaches redb some
        // other way — would be decoded, served, and re-promoted into L1 forever, and every read
        // path (`get_with_freshness`, and the prewarm's `load_recent`) goes through here. Refusing
        // and removing it costs one rebuild.
        if !cached.result.is_durable()
            || !crate::cache::key::fingerprints_are_reusable(&cached.dependency_fingerprints)
        {
            crate::logging::log_debug(
                "cache",
                format!(
                    "evicting a non-durable disk entry for {key} (stage: {})",
                    cached.result.unmeasured_stage().unwrap_or("none")
                ),
            );
            self.remove(key);
            return None;
        }
        // First-party-ness is key-derived; stamp it once at hydration so the
        // per-hit gate never has to re-decode the identity.
        cached.first_party = crate::cache::key::cache_key_is_first_party(key);
        // First-party (workspace / npm-link / file:) source files can be rewritten
        // equal-length with a preserved mtime, which the cheap pre-filter would miss
        // (X-7). Hash-verify them strictly on the cold disk-hydration path too, so the
        // blind spot isn't served even on the first hit before memory hydration.
        // node_modules files stay on the cheap pre-filter inside the strict variant.
        // Per FINGERPRINT, not per entry: a node_modules entry can carry a workspace file that a
        // stylesheet's `url()` reached outside the package root (D18). Routing by entry meant that
        // file's stored content hash was never consulted, and a rehydrate from disk re-armed the
        // same blind spot, so a restart did not clear it either.
        let freshness =
            crate::cache::key::check_fingerprints_strict(&cached.dependency_fingerprints);
        match freshness {
            crate::cache::key::Freshness::Stale | crate::cache::key::Freshness::Gone => {
                self.remove(key);
                None
            }
            // Fresh OR Unknown → keep and return the entry (Unknown must not delete).
            crate::cache::key::Freshness::Fresh | crate::cache::key::Freshness::Unknown => {
                Some((cached, freshness))
            }
        }
    }

    /// The current clear generation. A writer captures this BEFORE deriving the bytes
    /// it will queue; if a `clear()` bumps it in between, `flush_pending_inserts` drops
    /// those now-stale bytes so a wipe cannot be undone by an in-flight writer (RB-3).
    pub fn clear_generation(&self) -> u64 {
        self.clear_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Queues `cached` for a batched disk commit, tagged with the CURRENT clear
    /// generation. Correct for a fresh insert whose bytes are derived now; a
    /// snapshot-based writer must instead capture the generation before its snapshot
    /// and use [`Self::insert_at_generation`]. Returns `Ok(())` (a no-op) when the disk
    /// cache is disabled (memory-only mode has no byte budget).
    pub fn insert(&self, key: &str, cached: &CachedImport) -> Result<(), String> {
        self.insert_at_generation(key, cached, self.clear_generation())
    }

    /// Like [`Self::insert`], but tags the queued entry with a caller-captured clear
    /// `generation` rather than the current one. The `flush_to_disk` dirty replay +
    /// recency sweep and `enforce_memory_cap`'s re-persist derive their bytes from a
    /// memory snapshot taken earlier, so they capture the generation BEFORE that
    /// snapshot and pass it here: a `clear()` landing between the snapshot and the
    /// enqueue then bumps the generation, and this entry — still carrying the old one —
    /// is dropped by `flush_pending_inserts` instead of resurrecting the wiped shard.
    ///
    /// **The transience gate is applied here too**, and not merely upstream in `ImportCache`
    /// (ADR-0006, invariant 3). L2 is a store in its own right — it outlives the process, which is
    /// the worst place for a scheduling accident to land — and "the caller already checked" is the
    /// assumption that produced this defect six times. A refused insert is a no-op, not an error:
    /// `Err` here marks the key dirty for a flush replay, which would defeat the refusal.
    pub fn insert_at_generation(
        &self,
        key: &str,
        cached: &CachedImport,
        generation: u64,
    ) -> Result<(), String> {
        if self.db_read().is_none() {
            return Ok(());
        }
        if !cached.result.is_durable()
            || !crate::cache::key::fingerprints_are_reusable(&cached.dependency_fingerprints)
        {
            crate::logging::log_debug(
                "cache",
                format!(
                    "refusing to persist a non-durable result for {key} (stage: {})",
                    cached.result.unmeasured_stage().unwrap_or("none")
                ),
            );
            return Ok(());
        }

        self.write_at_generation(key, cached, generation)
    }

    /// The write, with the durability gate already applied — or, in a test, deliberately not.
    ///
    /// The gate protects L2 from what it is handed *today*. It says nothing about a row a build
    /// that predates it already wrote, and that row is on real users' disks right now. This is the
    /// only way to put one there, and it is `#[cfg(test)]` so it stays the only way.
    #[cfg(test)]
    pub(crate) fn write_ungated_for_test(
        &self,
        key: &str,
        cached: &CachedImport,
    ) -> Result<(), String> {
        if self.db_read().is_none() {
            return Ok(());
        }

        self.write_at_generation(key, cached, self.clear_generation())
    }

    fn write_at_generation(
        &self,
        key: &str,
        cached: &CachedImport,
        generation: u64,
    ) -> Result<(), String> {
        #[cfg(test)]
        {
            test_support::record_insert_attempt(key);
            if test_support::should_fail_insert(key) {
                return Err(format!("forced cache insert failure for {key}"));
            }
        }
        // An insert is shard activity; stamp the idle gate's last-access clock
        // (after the disabled short-circuit, so a no-op cache skips the syscall).
        self.stamp_access();

        let mut persisted = cached.clone();
        persisted.result.cache_hit = false;

        let bytes = encode_cache_value(persisted)?;

        // Queue for a batched commit instead of one durable transaction per
        // entry; a cold parallel batch otherwise serialized N fsyncs on redb's
        // single writer.
        let should_flush = match self.pending_inserts.lock() {
            Ok(mut pending) => {
                pending.insert(key.to_owned(), (generation, bytes));
                pending.len() >= INSERT_FLUSH_BATCH
            }
            Err(_) => return Err("cache pending-insert lock poisoned".to_owned()),
        };
        if should_flush {
            self.flush_pending_inserts();
        }
        Ok(())
    }

    pub fn flush_pending_inserts(&self) {
        // Serialize against `clear()` so its (bump generation + wipe) and this
        // (read generation + write) can never interleave: this flush runs entirely
        // before a clear (its writes are then wiped) or entirely after it (it observes
        // the bumped generation and drops every pre-clear entry). A poisoned lock is
        // recovered — a prior panic here must not wedge all future flushes (RB-3).
        let _clear_guard = self
            .clear_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return,
        };
        let pending = match self.pending_inserts.lock() {
            Ok(mut pending) => {
                if pending.is_empty() {
                    return;
                }
                std::mem::take(&mut *pending)
            }
            Err(_) => return,
        };

        // Drop every entry a `clear()` superseded after it was queued: only bytes
        // still carrying the current generation are written (RB-3). Read under
        // `clear_lock`, so the generation cannot change between here and the write.
        // Moves the kept bytes (no clone) to keep the batched flush cheap.
        let generation = self.clear_generation();
        let mut kept: HashMap<String, Vec<u8>> = HashMap::with_capacity(pending.len());
        for (key, (entry_generation, bytes)) in pending {
            if entry_generation == generation {
                kept.insert(key, bytes);
            }
        }
        if kept.is_empty() {
            return;
        }

        if let Err(error) = write_pending_inserts(db, &kept) {
            if let Ok(mut current) = self.pending_inserts.lock() {
                // Re-queue only the entries we tried to write, preserving their
                // (still-current) generation tag so a later flush retries them.
                for (key, bytes) in kept {
                    current.entry(key).or_insert((generation, bytes));
                }
            }
            cache_warn(format!("failed to flush cache inserts: {error}"));
        }
    }

    fn pending_insert_entry(
        &self,
        key: &str,
    ) -> Option<(CachedImport, crate::cache::key::Freshness)> {
        let bytes = {
            let pending = self.pending_inserts.lock().ok()?;
            // Value is `(clear_generation, bytes)`. Serve a queued-but-unflushed entry on
            // a get ONLY while its generation still matches: a clear() that superseded it
            // (bumping the generation) must not be undone by a get reading stale pending
            // bytes, exactly as `flush_pending_inserts` drops it before the disk (RB-3).
            let (entry_generation, bytes) = pending.get(key)?;
            if *entry_generation != self.clear_generation() {
                return None;
            }
            bytes.clone()
        };
        let mut cached = decode_cached_result(&bytes)?;
        cached.first_party = crate::cache::key::cache_key_is_first_party(key);
        // First-party (workspace / npm-link / file:) source files can be rewritten
        // equal-length with a preserved mtime, which the cheap pre-filter would miss
        // (X-7). Hash-verify them strictly on the cold disk-hydration path too, so the
        // blind spot isn't served even on the first hit before memory hydration.
        // node_modules files stay on the cheap pre-filter inside the strict variant.
        // Per FINGERPRINT, not per entry: a node_modules entry can carry a workspace file that a
        // stylesheet's `url()` reached outside the package root (D18). Routing by entry meant that
        // file's stored content hash was never consulted, and a rehydrate from disk re-armed the
        // same blind spot, so a restart did not clear it either.
        let freshness =
            crate::cache::key::check_fingerprints_strict(&cached.dependency_fingerprints);
        match freshness {
            crate::cache::key::Freshness::Stale | crate::cache::key::Freshness::Gone => {
                self.remove(key);
                return None;
            }
            // Fresh OR Unknown → keep and return the entry (Unknown must not delete).
            crate::cache::key::Freshness::Fresh | crate::cache::key::Freshness::Unknown => {}
        }
        Some((cached, freshness))
    }

    pub fn remove(&self, key: &str) {
        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.remove(key);
        }
        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            match maintain_removals(&write_txn, std::iter::once(key)) {
                Ok(_) => {
                    let _ = write_txn.commit();
                }
                Err(error) => {
                    cache_warn(format!("failed to remove cache entry: {error}"));
                    let _ = write_txn.abort();
                }
            }
        }
    }

    /// Returns the `limit` most-recently-used keys, highest `last_seq` first.
    /// Recency now lives in each entry's envelope (there is no separate recents
    /// table), so this scans CACHE_TABLE and decodes each value's `last_seq`. Used
    /// only at startup preload and prewarm, so the full scan is off the hot path
    /// (it shares the same cost model as the byte-budget rollup scan).
    pub fn recent_keys(&self, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }

        self.flush_pending_inserts();

        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return Vec::new(),
        };
        let read_txn = match db.begin_read() {
            Ok(txn) => txn,
            Err(error) => {
                cache_warn(format!("failed to begin recent cache read: {error}"));
                return Vec::new();
            }
        };
        let table = match read_txn.open_table(CACHE_TABLE) {
            Ok(table) => table,
            Err(error) => {
                cache_warn(format!("failed to open cache table: {error}"));
                return Vec::new();
            }
        };
        let iter = match table.iter() {
            Ok(iter) => iter,
            Err(error) => {
                cache_warn(format!("failed to iterate cache table: {error}"));
                return Vec::new();
            }
        };
        let mut keys = iter
            .filter_map(|entry| {
                let (key, value) = entry.ok()?;
                let last_seq = decode_last_seq(value.value());
                Some((key.value().to_owned(), last_seq))
            })
            .collect::<Vec<_>>();

        if keys.len() > limit {
            keys.select_nth_unstable_by(limit, compare_recent_keys);
            keys.truncate(limit);
        }
        keys.sort_by(compare_recent_keys);
        keys.into_iter().map(|(key, _)| key).collect()
    }

    /// One-pass summary of this shard for the byte-budget coordinator: total
    /// on-disk bytes, the oldest recency sequence held, and the entry count. Built
    /// once when the shard is loaded (the coordinator then maintains it
    /// incrementally on insert/evict rather than rescanning per operation). Size is
    /// the exact CACHE_TABLE value length; recency is each envelope's `last_seq`.
    /// Also advances the recency clock past every persisted seq so a post-restart
    /// access sorts newer than durable entries.
    pub fn shard_rollup(&self) -> ShardRollup {
        self.flush_pending_inserts();

        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return ShardRollup::empty(),
        };
        let read_txn = match db.begin_read() {
            Ok(txn) => txn,
            Err(error) => {
                cache_warn(format!("failed to begin rollup read: {error}"));
                return ShardRollup::empty();
            }
        };
        let summary = match read_txn.open_table(SUMMARY_TABLE) {
            Ok(summary) => summary,
            Err(error) => {
                cache_warn(format!("failed to open summary table for rollup: {error}"));
                return ShardRollup::empty();
            }
        };

        let total_bytes = read_summary_field(&summary, SUMMARY_TOTAL_BYTES);
        let entry_count = read_summary_field(&summary, SUMMARY_ENTRY_COUNT);
        // Keep the live recency clock ahead of every persisted seq. The scan used
        // to observe each entry's seq; the summary's high-water does it in O(1).
        crate::cache::recency::RecencyClock::observe(read_summary_field(&summary, SUMMARY_MAX_SEQ));

        if entry_count == 0 {
            return ShardRollup::empty();
        }

        // `oldest_seq` is the first key of the ascending `(last_seq, key)` index —
        // an O(log N) descent rather than a full min-scan.
        let oldest_seq = match read_txn.open_table(SEQ_INDEX_TABLE) {
            Ok(seq_index) => match seq_index.first() {
                Ok(Some((key, _))) => key.value().0,
                Ok(None) => u64::MAX,
                Err(error) => {
                    cache_warn(format!("failed to read oldest seq from index: {error}"));
                    u64::MAX
                }
            },
            Err(error) => {
                cache_warn(format!("failed to open seq index for rollup: {error}"));
                u64::MAX
            }
        };

        ShardRollup {
            total_bytes,
            oldest_seq,
            entry_count,
        }
    }

    /// The largest `last_seq` persisted in this shard — a single-key read of the
    /// SUMMARY `max_seq` high-water, NOT a CACHE_TABLE scan. Used by the startup
    /// recency seed (C5 / Finding 10d, §3.3) to lift the process-global clock above
    /// every persisted seq BEFORE serving, without paying for a full `shard_rollup`
    /// (which also descends the seq index for `oldest_seq`). Returns `0` for a
    /// fresh/empty shard or an unavailable database. Flushes queued inserts first so
    /// a not-yet-committed high seq is included, mirroring `shard_rollup`.
    pub fn summary_max_seq(&self) -> u64 {
        self.flush_pending_inserts();

        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return 0,
        };
        let Ok(read_txn) = db.begin_read() else {
            return 0;
        };
        let Ok(summary) = read_txn.open_table(SUMMARY_TABLE) else {
            return 0;
        };
        read_summary_field(&summary, SUMMARY_MAX_SEQ)
    }

    /// Returns up to `n` of the shard's lowest-`last_seq` (least-recently-used)
    /// keys with their PERSISTED seq, EXCLUDING the shard's `floor` highest-seq
    /// entries (the per-project floor, so a small project keeps its newest
    /// working set even when a larger project drives global eviction). Empty when
    /// every entry is within the floor. The persisted seq lets a caller with a
    /// memory layer detect entries promoted since their last persist and shield
    /// them from eviction.
    pub fn lowest_seq_keys(&self, n: usize, floor: u64) -> Vec<(String, u64)> {
        if n == 0 {
            return Vec::new();
        }
        self.flush_pending_inserts();

        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return Vec::new(),
        };
        let Ok(read_txn) = db.begin_read() else {
            return Vec::new();
        };
        let Ok(summary) = read_txn.open_table(SUMMARY_TABLE) else {
            return Vec::new();
        };

        // The `floor` highest-seq entries are protected; the eligible set is
        // everything past the floor. `take` bounds the range read so a huge shard
        // never materializes more than the evictor asked for.
        let entry_count = read_summary_field(&summary, SUMMARY_ENTRY_COUNT);
        let take = n.min(entry_count.saturating_sub(floor) as usize);
        if take == 0 {
            return Vec::new();
        }

        let Ok(seq_index) = read_txn.open_table(SEQ_INDEX_TABLE) else {
            return Vec::new();
        };
        let Ok(iter) = seq_index.iter() else {
            return Vec::new();
        };

        // The index is ascending by `(last_seq, key)`, so the first `take` rows
        // are exactly the lowest-seq keys beyond the floor (seq, then key, as the
        // old sort-scan tiebreak did) — O(take · log N), no full materialization.
        let mut lowest = Vec::with_capacity(take);
        for entry in iter {
            let Ok((key_guard, _)) = entry else { continue };
            let (seq, key) = key_guard.value();
            lowest.push((key.to_owned(), seq));
            if lowest.len() >= take {
                break;
            }
        }
        lowest
    }

    /// Deletes `keys` from the shard in one write transaction and returns the total
    /// on-disk bytes freed (summed CACHE_TABLE value lengths of the removed rows).
    pub fn remove_keys(&self, keys: &[String]) -> u64 {
        if keys.is_empty() {
            return 0;
        }
        if let Ok(mut pending) = self.pending_inserts.lock() {
            for key in keys {
                pending.remove(key);
            }
        }
        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return 0,
        };

        let Ok(write_txn) = db.begin_write() else {
            return 0;
        };
        // The freed byte count is the summed removed value lengths (see
        // `maintain_removals`), so it matches the rollup accounting exactly. Only
        // report bytes the commit actually durably freed.
        match maintain_removals(&write_txn, keys.iter().map(String::as_str)) {
            Ok(freed) => match write_txn.commit() {
                Ok(()) => freed,
                Err(error) => {
                    cache_warn(format!("failed to commit cache removal: {error}"));
                    0
                }
            },
            Err(error) => {
                cache_warn(format!("failed to remove cache entries: {error}"));
                let _ = write_txn.abort();
                0
            }
        }
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.invalidate_packages(&HashSet::from([package_name.to_owned()]));
    }

    /// Evicts every entry belonging to any package in `package_names` in a single
    /// table scan that decodes each key once, rather than one full scan (with a
    /// per-key decode) per package.
    pub fn invalidate_packages(&self, package_names: &HashSet<String>) {
        if package_names.is_empty() {
            return;
        }

        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.retain(|key, _| !cache_key_matches_any_package(key, package_names));
        }
        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            // Collect the matching keys under this txn, dropping the read-only
            // table handle before `maintain_removals` re-opens CACHE_TABLE (redb
            // forbids opening the same table twice in one write transaction).
            let keys_to_remove = {
                let mut keys = Vec::new();
                if let Ok(table) = write_txn.open_table(CACHE_TABLE)
                    && let Ok(iter) = table.iter()
                {
                    for result in iter {
                        if let Ok((key, _)) = result
                            && cache_key_matches_any_package(key.value(), package_names)
                        {
                            keys.push(key.value().to_owned());
                        }
                    }
                }
                keys
            };

            // A CACHE_TABLE mutator like any other: maintain SUMMARY + SEQ_INDEX
            // in the same txn so invalidation never drifts the accounting.
            match maintain_removals(&write_txn, keys_to_remove.iter().map(String::as_str)) {
                Ok(_) => {
                    let _ = write_txn.commit();
                }
                Err(error) => {
                    cache_warn(format!("failed to invalidate cache entries: {error}"));
                    let _ = write_txn.abort();
                }
            }
        }
    }

    /// Reclaims redb free pages when the shard is IDLE and its free-space ratio
    /// exceeds `threshold` (e.g. 0.5 = over half the file is reclaimable). `redb`
    /// reuses freed pages rather than shrinking the file, so after heavy eviction
    /// the `.redb` file can far exceed the logical byte budget until compacted.
    ///
    /// Two-stage gating keeps the common path cheap and the user's gets unblocked
    /// (§5.5 / Finding 12):
    ///  1. Idle gate — a single relaxed load. A shard touched by a get/insert
    ///     within `COMPACT_IDLE` is skipped outright, because `Database::compact`
    ///     holds the exclusive lock across the whole rewrite and would stall the
    ///     user's concurrent gets on a shard they are actively analyzing.
    ///  2. Lock-free probe — the fragmentation ratio is read under the SHARED
    ///     `db_read()` guard (`fragmentation_ratio` needs only `&Database`), so a
    ///     non-fragmented shard never pays the exclusive lock merely to be
    ///     checked. The shared guard is dropped before escalating (std `RwLock`
    ///     cannot upgrade a read guard to a write guard on the same thread).
    ///
    /// Only when both gates pass does it escalate to the exclusive `db.write()`
    /// for the compact itself: `Database::compact` needs `&mut Database` and fails
    /// if any read transaction is live, and the write lock guarantees neither (all
    /// normal ops share the read lock). Runs off the hot path on the idle
    /// maintenance tick. Returns whether it compacted.
    pub fn compact_if_fragmented(&self, threshold: f64) -> bool {
        // Stage 1 — idle gate (cheapest possible check, one relaxed load): never
        // compact a shard the user is actively analyzing.
        let idle_for = Duration::from_millis(
            crate::time::unix_millis_now()
                .saturating_sub(self.last_access.load(std::sync::atomic::Ordering::Relaxed)),
        );
        if idle_for < COMPACT_IDLE {
            return false;
        }

        // Stage 2 — lock-free fragmentation probe under the SHARED read guard, so
        // a non-fragmented shard is never charged the exclusive lock just to be
        // checked. Drop the guard before escalating: std `RwLock` cannot upgrade a
        // held read guard to the write guard on the same thread.
        let free_ratio = {
            let db_guard = self.db_read();
            let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
                Some(db) => db,
                None => return false,
            };
            fragmentation_ratio(db)
        };
        if free_ratio <= threshold {
            return false;
        }

        // Both gates passed — escalate to the exclusive guard for the compact.
        let mut guard = self.db.write().unwrap_or_else(|poison| poison.into_inner());
        let Some(database) = guard.as_mut() else {
            return false;
        };

        // Recompute under the exclusive guard: the shared-guard ratio was a
        // pre-lock estimate (a concurrent insert/evict could have moved it since),
        // and recomputing avoids compacting on a now-stale decision.
        let free_ratio = fragmentation_ratio(database);
        if free_ratio <= threshold {
            return false;
        }

        // Compaction rewrites the live dataset; log its duration so the
        // "acceptably brief while holding the exclusive lock" assumption stays
        // observable in the field.
        let started = std::time::Instant::now();
        match database.compact() {
            Ok(compacted) => {
                if compacted {
                    crate::logging::log_debug(
                        "cache",
                        format!(
                            "compacted shard in {} ms (free ratio {:.2})",
                            started.elapsed().as_millis(),
                            free_ratio
                        ),
                    );
                }
                compacted
            }
            Err(error) => {
                cache_warn(format!("failed to compact cache database: {error}"));
                false
            }
        }
    }

    /// Drops orphaned entries (release-stale analyzer version, or a resolved
    /// package/entry path that no longer exists). Scans once under a read txn,
    /// then removes under a short write txn. Returns the number removed.
    pub fn purge_orphan_entries(&self, current_analyzer_version: &str) -> usize {
        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return 0,
        };

        let mut orphan_keys = Vec::new();
        if let Ok(read_txn) = db.begin_read()
            && let Ok(table) = read_txn.open_table(CACHE_TABLE)
            && let Ok(iter) = table.iter()
        {
            for result in iter {
                if let Ok((key, _)) = result
                    && cache_key_is_orphan(key.value(), current_analyzer_version)
                {
                    orphan_keys.push(key.value().to_owned());
                }
            }
        }

        if orphan_keys.is_empty() {
            return 0;
        }

        // The scan ran under a read txn; the removal opens its own write txn (no
        // same-table double-open), and maintains SUMMARY + SEQ_INDEX in it.
        let mut removed = 0;
        if let Ok(write_txn) = db.begin_write() {
            match maintain_removals(&write_txn, orphan_keys.iter().map(String::as_str)) {
                Ok(_) => {
                    if write_txn.commit().is_ok() {
                        removed = orphan_keys.len();
                    } else {
                        cache_warn("failed to commit orphan cache purge".to_owned());
                    }
                }
                Err(error) => {
                    cache_warn(format!("failed to purge orphan cache entries: {error}"));
                    let _ = write_txn.abort();
                }
            }
        }

        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.retain(|key, _| !cache_key_is_orphan(key, current_analyzer_version));
        }

        removed
    }

    pub fn clear(&self) {
        // Serialize against `flush_pending_inserts` (see `clear_lock`) and bump the
        // clear generation FIRST — before the wipe and before dropping pending — so any
        // writer that already captured the old generation is superseded: its queued
        // bytes fail the flush's generation filter, and the ImportCache memory-rollback
        // guard (which reads this generation) sees the change (RB-3). Bump even when the
        // disk is disabled: memory-only mode has no `db` to wipe but still relies on the
        // generation for that rollback. Recover a poisoned lock so a prior panic cannot
        // wedge every future clear.
        let _clear_guard = self
            .clear_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.clear_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        let db_guard = self.db_read();
        if let Some(db) = db_guard.as_ref().and_then(|guard| guard.as_ref())
            && let Ok(write_txn) = db.begin_write()
        {
            // Drop every row from all three tables and zero the summary, so a
            // cleared shard rolls up as empty with no orphaned index entries.
            if let Ok(mut cache) = write_txn.open_table(CACHE_TABLE) {
                let _ = cache.retain(|_, _| false);
            }
            if let Ok(mut seq_index) = write_txn.open_table(SEQ_INDEX_TABLE) {
                let _ = seq_index.retain(|_, _| false);
            }
            if let Ok(mut summary) = write_txn.open_table(SUMMARY_TABLE) {
                let _ = summary.insert(SUMMARY_TOTAL_BYTES, 0);
                let _ = summary.insert(SUMMARY_ENTRY_COUNT, 0);
                let _ = summary.insert(SUMMARY_MAX_SEQ, 0);
            }
            let _ = write_txn.commit();
        }
        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.clear();
        }
    }

    /// Recomputes SUMMARY (`total_bytes`/`entry_count`/`max_seq`) and rebuilds
    /// SEQ_INDEX from a full CACHE_TABLE scan, writing them authoritatively. The
    /// drift oracle for tests and the heal fallback if incremental maintenance
    /// ever misses a mutation site. No-op when disk caching is disabled.
    pub fn rebuild_summary_from_scan(&self) {
        // Flush first so the scan sees every queued insert, exactly as the read
        // paths do before consulting the summary.
        self.flush_pending_inserts();

        let db_guard = self.db_read();
        let db = match db_guard.as_ref().and_then(|guard| guard.as_ref()) {
            Some(db) => db,
            None => return,
        };
        let Ok(write_txn) = db.begin_write() else {
            return;
        };
        match rebuild_summary_in_txn(&write_txn) {
            Ok(()) => {
                if let Err(error) = write_txn.commit() {
                    cache_warn(format!("failed to commit cache summary rebuild: {error}"));
                }
            }
            Err(error) => {
                cache_warn(format!("failed to rebuild cache summary: {error}"));
                let _ = write_txn.abort();
            }
        }
    }

    /// Rebuilds the summary/index when the persisted `entry_count` disagrees with
    /// the actual CACHE_TABLE row count. The len check is O(1) (redb tracks both),
    /// so a correctly-maintained shard pays only that; the O(N) scan runs once,
    /// only when there is genuine drift or an absent summary to heal.
    fn heal_summary_if_inconsistent(db: &Database) {
        let needs_rebuild = match db.begin_read() {
            Ok(read_txn) => {
                let cache_len = read_txn
                    .open_table(CACHE_TABLE)
                    .ok()
                    .and_then(|table| table.len().ok())
                    .unwrap_or(0);
                let summary_count = match read_txn.open_table(SUMMARY_TABLE) {
                    Ok(summary) => read_summary_field(&summary, SUMMARY_ENTRY_COUNT),
                    Err(_) => 0,
                };
                cache_len != summary_count
            }
            Err(_) => false,
        };
        if !needs_rebuild {
            return;
        }

        if let Ok(write_txn) = db.begin_write() {
            match rebuild_summary_in_txn(&write_txn) {
                Ok(()) => {
                    if let Err(error) = write_txn.commit() {
                        cache_warn(format!("failed to commit cache summary heal: {error}"));
                    }
                }
                Err(error) => {
                    cache_warn(format!("failed to heal cache summary: {error}"));
                    let _ = write_txn.abort();
                }
            }
        }
    }

    fn disabled() -> Self {
        Self {
            db: RwLock::new(None),
            pending_inserts: Mutex::new(HashMap::new()),
            clear_generation: AtomicU64::new(0),
            clear_lock: Mutex::new(()),
            // A disabled cache never compacts (no database), so the value is
            // immaterial; 0 (the `Default` for the enabled-but-never-opened case
            // too) simply reads as idle.
            last_access: AtomicU64::new(0),
        }
    }

    fn open_database(storage_path: &Path) -> Option<Database> {
        if let Err(error) = fs::create_dir_all(storage_path) {
            cache_warn(format!(
                "failed to create cache directory {}: {error}",
                storage_path.display()
            ));
            return None;
        }

        let db_path = storage_path.join(CACHE_DB_FILE_NAME);
        let db_existed = db_path.exists();
        let db = match Database::create(&db_path) {
            Ok(db) => db,
            // The file is already open elsewhere in this process (redb allows one
            // Database per file). This happens when a temp open for a maintenance
            // pass — eviction, invalidation, orphan purge — races the same shard
            // being loaded. NEVER recreate here: `recreate_database` unlinks the
            // file, which would destroy the live shard's data. Degrade to a
            // disabled cache for this transient open instead.
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                cache_warn(format!(
                    "cache database {} is already open; skipping this open",
                    db_path.display()
                ));
                return None;
            }
            Err(error) => {
                cache_warn(format!(
                    "failed to open cache database {}: {error}",
                    db_path.display()
                ));
                // Only a genuine corruption / unrecoverable-format signal justifies
                // unlinking the shard. A transient open failure (Windows sharing
                // violation, AV lock, permission blip, flaky/offline drive) keeps
                // the possibly-valid file so a later open retries (§12 / X-5).
                if Self::is_corruption_error(&error) {
                    return Self::recreate_database(&db_path);
                }
                return None;
            }
        };

        match Self::ensure_schema(&db, !db_existed) {
            Ok(()) => {
                // Cheap O(1) drift check on open: if the persisted entry_count
                // disagrees with the CACHE_TABLE row count (a drifted shard, or a
                // v7 shard whose summary rows are absent), rebuild once from a
                // scan. A correctly-maintained shard matches and never scans; a
                // v6→v7 wipe recreates empty and also matches at zero.
                Self::heal_summary_if_inconsistent(&db);
                Some(db)
            }
            // A recognized-but-incompatible schema (wrong version, or an existing
            // database with no version key) is the sanctioned migration wipe.
            Err(SchemaError::Incompatible(reason)) => {
                cache_warn(format!(
                    "cache database {} has an incompatible schema, recreating: {reason}",
                    db_path.display()
                ));
                drop(db);
                Self::recreate_database(&db_path)
            }
            // A transient schema-read failure keeps the (possibly valid) DB so a
            // later open retries rather than wiping good data (§12 / X-5).
            Err(SchemaError::Transient(message)) => {
                cache_warn(format!(
                    "cache database {} schema check failed transiently, keeping it: {message}",
                    db_path.display()
                ));
                None
            }
        }
    }

    /// True only for a genuine on-disk corruption / unrecoverable-format signal
    /// that justifies wiping+recreating the shard. A transient open failure
    /// (lock, AV, permission, IO on a flaky/offline drive) is NOT corruption — it
    /// must keep the (possibly valid) DB and retry later. See §12 / X-5.
    ///
    /// redb 4.1's `DatabaseError` / `StorageError` are `#[non_exhaustive]`, so the
    /// catch-all keeps every unclassified error (including any future variant) on
    /// the safe "keep" side — never `_ => true`, which would resurrect the bug.
    pub(crate) fn is_corruption_error(error: &redb::DatabaseError) -> bool {
        use redb::{DatabaseError, StorageError};
        match error {
            // redb detected a corrupted on-disk structure it could not recover.
            DatabaseError::Storage(StorageError::Corrupted(_)) => true,
            // A valid file in an older on-disk format redb can no longer open, with
            // no automatic migration. For a rebuildable cache the sanctioned
            // recovery is the same wipe-and-recreate as a schema-version mismatch.
            DatabaseError::UpgradeRequired(_) => true,
            // The database needed repair and repair did not complete (reachable
            // only via an aborting repair callback or a read-only open, neither of
            // which `Database::create` installs — so this is defensive). The shard
            // is unusable as-is and this is never a transient lock/permission/IO
            // fault, so recreate.
            DatabaseError::RepairAborted => true,
            // A bad/absent magic number ("not a redb database") is surfaced as an
            // IO error of kind `InvalidData`, and that is the ONLY `InvalidData`
            // redb produces while opening. It is a format-corruption signal: a
            // transient fault (sharing violation, AV lock, permission blip,
            // flaky/offline drive) surfaces under a DIFFERENT `ErrorKind`, so every
            // other IO kind is kept.
            DatabaseError::Storage(StorageError::Io(source)) => {
                source.kind() == std::io::ErrorKind::InvalidData
            }
            // Not positively corruption → KEEP the possibly-valid database:
            //   DatabaseAlreadyOpen     - concurrent open; handled before this call
            //   TransactionInProgress   - transient lifecycle state
            //   Storage(ValueTooLarge)  - cannot occur while opening
            //   Storage(PreviousIo)     - transient IO; close and re-open
            //   Storage(DatabaseClosed) - transient lifecycle state
            //   Storage(LockPoisoned)   - a panic poisoned an internal lock, not disk
            // plus any future `#[non_exhaustive]` variant.
            _ => false,
        }
    }

    fn recreate_database(db_path: &Path) -> Option<Database> {
        if let Err(error) = fs::remove_file(db_path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                cache_warn(format!(
                    "failed to delete cache database {}: {error}",
                    db_path.display()
                ));
            }
            return None;
        }

        let db = match Database::create(db_path) {
            Ok(db) => db,
            Err(error) => {
                cache_warn(format!(
                    "failed to recreate cache database {}: {error}",
                    db_path.display()
                ));
                return None;
            }
        };

        if let Err(error) = Self::ensure_schema(&db, true) {
            cache_warn(format!(
                "failed to initialize cache database {}: {error}",
                db_path.display()
            ));
            return None;
        }

        Some(db)
    }

    fn ensure_schema(db: &Database, initialize_missing_schema: bool) -> Result<(), SchemaError> {
        let write_txn = db.begin_write().map_err(|error| {
            SchemaError::Transient(format!("failed to begin schema transaction: {error}"))
        })?;

        let version = {
            let mut metadata = write_txn.open_table(METADATA_TABLE).map_err(|error| {
                SchemaError::Transient(format!("failed to open metadata table: {error}"))
            })?;
            let current = metadata
                .get(SCHEMA_VERSION_KEY)
                .map_err(|error| {
                    SchemaError::Transient(format!("failed to read schema version: {error}"))
                })?
                .map(|value| value.value());

            match current {
                Some(value) => value,
                None if initialize_missing_schema => {
                    metadata
                        .insert(SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION)
                        .map_err(|error| {
                            SchemaError::Transient(format!(
                                "failed to write schema version: {error}"
                            ))
                        })?;
                    CURRENT_SCHEMA_VERSION
                }
                // An existing database with the metadata table but no version key
                // is a recognized-incompatible schema (pre-versioning or a wiped
                // row), not a transient fault → the migration wipe recreates it.
                None => {
                    return Err(SchemaError::Incompatible(
                        "schema version is missing".to_owned(),
                    ));
                }
            }
        };

        if version != CURRENT_SCHEMA_VERSION {
            return Err(SchemaError::Incompatible(format!(
                "schema version {version} does not match {CURRENT_SCHEMA_VERSION}"
            )));
        }

        {
            write_txn.open_table(CACHE_TABLE).map_err(|error| {
                SchemaError::Transient(format!("failed to open cache table: {error}"))
            })?;
            // Create the v7 accounting tables so a fresh shard has them and the
            // maintenance paths never race a missing-table open.
            write_txn.open_table(SUMMARY_TABLE).map_err(|error| {
                SchemaError::Transient(format!("failed to open summary table: {error}"))
            })?;
            write_txn.open_table(SEQ_INDEX_TABLE).map_err(|error| {
                SchemaError::Transient(format!("failed to open seq index table: {error}"))
            })?;
        }

        write_txn.commit().map_err(|error| {
            SchemaError::Transient(format!("failed to commit schema transaction: {error}"))
        })
    }
}

/// Why `ensure_schema` could not certify a database at the current schema
/// version. The two dispositions differ sharply — recreate vs. keep — so before
/// X-5 collapsing them to one error string wiped valid caches on a transient
/// schema-read blip.
enum SchemaError {
    /// A recognized but incompatible on-disk schema: the stored version differs
    /// from `CURRENT_SCHEMA_VERSION`, or an existing database carries no version
    /// key at all. Both are the sanctioned migration wipe — the shard is recreated
    /// empty. The schema was read successfully; this is NOT a transient fault.
    Incompatible(String),
    /// A transient failure while reading or writing the schema (begin/commit a
    /// transaction, open a table, read the version key). The database may be
    /// entirely valid, so it is kept and a later open retries rather than wiping
    /// possibly-good data.
    Transient(String),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::Incompatible(reason) | SchemaError::Transient(reason) => {
                f.write_str(reason)
            }
        }
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        self.flush_pending_inserts();
    }
}

/// The shard's reclaimable-free-space ratio (fragmented bytes / allocated bytes)
/// from redb's stats. redb exposes fragmentation only via `WriteTransaction::
/// stats()` — there is no `ReadTransaction::stats` — but `Database::begin_write`
/// needs just `&Database`, so this reads it under the caller's SHARED `db_read()`
/// guard, never the exclusive RwLock. The throwaway transaction is immediately
/// aborted and mutates nothing. Returns `0.0` (never fragmented) on any
/// transaction/stats error so a probe failure never provokes a compact.
fn fragmentation_ratio(db: &Database) -> f64 {
    let Ok(txn) = db.begin_write() else {
        return 0.0;
    };
    let ratio = match txn.stats() {
        Ok(stats) => {
            let allocated = stats.allocated_pages() * stats.page_size() as u64;
            if allocated == 0 {
                0.0
            } else {
                stats.fragmented_bytes() as f64 / allocated as f64
            }
        }
        Err(_) => 0.0,
    };
    let _ = txn.abort();
    ratio
}

fn write_pending_inserts(db: &Database, pending: &HashMap<String, Vec<u8>>) -> Result<(), String> {
    let write_txn = db
        .begin_write()
        .map_err(|error| format!("failed to begin cache write: {error}"))?;

    {
        let mut cache = write_txn
            .open_table(CACHE_TABLE)
            .map_err(|error| format!("failed to open cache table: {error}"))?;
        let mut summary = write_txn
            .open_table(SUMMARY_TABLE)
            .map_err(|error| format!("failed to open summary table: {error}"))?;
        let mut seq_index = write_txn
            .open_table(SEQ_INDEX_TABLE)
            .map_err(|error| format!("failed to open seq index table: {error}"))?;

        // Fold every entry's delta into locals, then write the summary once.
        // `total_bytes` is accumulated as i128 so a replace with a smaller value
        // never underflows before the final clamp back to u64.
        let mut total_bytes = read_summary_field(&summary, SUMMARY_TOTAL_BYTES) as i128;
        let mut entry_count = read_summary_field(&summary, SUMMARY_ENTRY_COUNT) as i128;
        let mut max_seq = read_summary_field(&summary, SUMMARY_MAX_SEQ);

        for (key, bytes) in pending {
            let new_len = bytes.len() as u64;
            let new_seq = decode_last_seq(bytes);

            // `insert` returns the prior value; read its length + seq before it
            // drops so the index/byte maintenance sees the exact replaced row.
            let prior = cache
                .insert(key.as_str(), bytes.as_slice())
                .map_err(|error| format!("failed to insert cache entry: {error}"))?;
            let prior_len_seq = prior.map(|value| {
                let old = value.value();
                (decode_last_seq(old), old.len() as u64)
            });

            if let Some((old_seq, old_len)) = prior_len_seq {
                // Replace: drop the stale index entry, adjust bytes, keep count.
                seq_index
                    .remove((old_seq, key.as_str()))
                    .map_err(|error| format!("failed to remove stale seq index entry: {error}"))?;
                total_bytes += new_len as i128 - old_len as i128;
            } else {
                total_bytes += new_len as i128;
                entry_count += 1;
            }
            seq_index
                .insert((new_seq, key.as_str()), ())
                .map_err(|error| format!("failed to insert seq index entry: {error}"))?;
            max_seq = max_seq.max(new_seq);
        }

        write_summary(&mut summary, total_bytes, entry_count, Some(max_seq))?;
    }

    write_txn
        .commit()
        .map_err(|error| format!("failed to commit cache write: {error}"))
}

/// Reads a `u64` summary field, defaulting to `0` when the key is absent (a fresh
/// v7 shard has no summary rows until its first insert).
fn read_summary_field<T: ReadableTable<&'static str, u64>>(table: &T, field: &str) -> u64 {
    table
        .get(field)
        .ok()
        .flatten()
        .map(|value| value.value())
        .unwrap_or(0)
}

/// Writes `total_bytes`/`entry_count` (clamped non-negative) back to SUMMARY, and
/// `max_seq` when supplied. Removals pass `None` for `max_seq` — the high-water
/// mark only advances on insert, so a removal must not lower it.
fn write_summary(
    summary: &mut redb::Table<'_, &'static str, u64>,
    total_bytes: i128,
    entry_count: i128,
    max_seq: Option<u64>,
) -> Result<(), String> {
    summary
        .insert(SUMMARY_TOTAL_BYTES, total_bytes.max(0) as u64)
        .map_err(|error| format!("failed to write summary total bytes: {error}"))?;
    summary
        .insert(SUMMARY_ENTRY_COUNT, entry_count.max(0) as u64)
        .map_err(|error| format!("failed to write summary entry count: {error}"))?;
    if let Some(max_seq) = max_seq {
        summary
            .insert(SUMMARY_MAX_SEQ, max_seq)
            .map_err(|error| format!("failed to write summary max seq: {error}"))?;
    }
    Ok(())
}

/// Removes `keys` from CACHE_TABLE inside `write_txn`, maintaining SUMMARY and
/// SEQ_INDEX in the SAME transaction, and returns the total on-disk bytes freed.
/// The caller owns the commit, so an aborted txn leaves data and accounting
/// consistent (all-or-nothing). `max_seq` is deliberately left untouched — it is
/// a high-water mark, and rescanning to lower it would defeat the O(1) intent.
fn maintain_removals<'a>(
    write_txn: &WriteTransaction,
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<u64, String> {
    let mut cache = write_txn
        .open_table(CACHE_TABLE)
        .map_err(|error| format!("failed to open cache table: {error}"))?;
    let mut summary = write_txn
        .open_table(SUMMARY_TABLE)
        .map_err(|error| format!("failed to open summary table: {error}"))?;
    let mut seq_index = write_txn
        .open_table(SEQ_INDEX_TABLE)
        .map_err(|error| format!("failed to open seq index table: {error}"))?;

    let mut total_bytes = read_summary_field(&summary, SUMMARY_TOTAL_BYTES) as i128;
    let mut entry_count = read_summary_field(&summary, SUMMARY_ENTRY_COUNT) as i128;
    let mut freed = 0_u64;

    for key in keys {
        let removed = cache
            .remove(key)
            .map_err(|error| format!("failed to remove cache entry: {error}"))?;
        let removed_len_seq = removed.map(|value| {
            let old = value.value();
            (decode_last_seq(old), old.len() as u64)
        });
        if let Some((seq, len)) = removed_len_seq {
            seq_index
                .remove((seq, key))
                .map_err(|error| format!("failed to remove seq index entry: {error}"))?;
            total_bytes -= len as i128;
            entry_count -= 1;
            freed += len;
        }
    }

    write_summary(&mut summary, total_bytes, entry_count, None)?;
    Ok(freed)
}

/// Recomputes SUMMARY and rebuilds SEQ_INDEX from a full CACHE_TABLE scan inside
/// `write_txn` (caller commits). The authoritative source of truth: the drift
/// oracle for tests and the heal fallback if incremental maintenance ever misses
/// a site.
fn rebuild_summary_in_txn(write_txn: &WriteTransaction) -> Result<(), String> {
    let cache = write_txn
        .open_table(CACHE_TABLE)
        .map_err(|error| format!("failed to open cache table: {error}"))?;
    let mut summary = write_txn
        .open_table(SUMMARY_TABLE)
        .map_err(|error| format!("failed to open summary table: {error}"))?;
    let mut seq_index = write_txn
        .open_table(SEQ_INDEX_TABLE)
        .map_err(|error| format!("failed to open seq index table: {error}"))?;

    // Repopulate the index from scratch so a stale/partial index self-heals.
    seq_index
        .retain(|_, _| false)
        .map_err(|error| format!("failed to clear seq index table: {error}"))?;

    let mut total_bytes = 0_u64;
    let mut entry_count = 0_u64;
    let mut max_seq = 0_u64;
    {
        let iter = cache
            .iter()
            .map_err(|error| format!("failed to iterate cache table: {error}"))?;
        for entry in iter {
            let (key, value) =
                entry.map_err(|error| format!("failed to read cache entry: {error}"))?;
            let bytes = value.value();
            let seq = decode_last_seq(bytes);
            total_bytes += bytes.len() as u64;
            entry_count += 1;
            max_seq = max_seq.max(seq);
            seq_index
                .insert((seq, key.value()), ())
                .map_err(|error| format!("failed to insert seq index entry: {error}"))?;
        }
    }

    write_summary(
        &mut summary,
        total_bytes as i128,
        entry_count as i128,
        Some(max_seq),
    )
}

// Orders `(key, last_seq)` pairs highest-`last_seq` first (most recent), with the
// key as a stable tiebreak. Used by `recent_keys` for preload/prewarm ordering.
fn compare_recent_keys(left: &(String, u64), right: &(String, u64)) -> Ordering {
    right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0))
}

/// Reads just the recency sequence from a stored value's fixed 8-byte prefix.
/// Returns `0` (oldest) for a value too short to carry the prefix, so a corrupt
/// row sorts as the first eviction candidate.
fn decode_last_seq(bytes: &[u8]) -> u64 {
    bytes
        .get(..SEQ_PREFIX_LEN)
        .and_then(|prefix| prefix.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

/// Serializes `cached` into the full CACHE_TABLE value:
/// `[last_seq LE][msgpack envelope]`.
fn encode_cache_value(cached: CachedImport) -> Result<Vec<u8>, String> {
    let last_seq = cached.last_seq.load(std::sync::atomic::Ordering::Relaxed);
    let envelope = CacheEnvelope {
        analyzer_version: ANALYZER_VERSION.to_owned(),
        full_contributions: if cached.result.internal_contributions.is_empty() {
            cached.result.module_breakdown.clone().unwrap_or_default()
        } else {
            cached.result.internal_contributions.clone()
        },
        result: cached.result,
        dependency_fingerprints: cached.dependency_fingerprints,
    };
    let envelope_bytes = rmp_serde::to_vec(&envelope)
        .map_err(|error| format!("failed to serialize cache entry: {error}"))?;

    let mut value = Vec::with_capacity(SEQ_PREFIX_LEN + envelope_bytes.len());
    value.extend_from_slice(&last_seq.to_le_bytes());
    value.extend_from_slice(&envelope_bytes);
    Ok(value)
}

/// Decodes a full CACHE_TABLE value. `first_party` is key-derived, so the caller
/// (which has the key) stamps it after decode; it defaults to `false` here.
fn decode_cached_result(bytes: &[u8]) -> Option<CachedImport> {
    let envelope_bytes = bytes.get(SEQ_PREFIX_LEN..)?;
    let last_seq = decode_last_seq(bytes);
    let envelope = rmp_serde::from_slice::<CacheEnvelope>(envelope_bytes).ok()?;
    if envelope.analyzer_version != ANALYZER_VERSION {
        return None;
    }

    let mut result = envelope.result;
    result.internal_contributions = envelope.full_contributions;
    // Keep the live recency clock ahead of every persisted seq so a
    // post-restart access can't sort as older than a durable entry.
    crate::cache::recency::RecencyClock::observe(last_seq);
    Some(CachedImport {
        result,
        dependency_fingerprints: envelope.dependency_fingerprints,
        verified_generation: 0,
        verified_at: None,
        first_party: false,
        last_seq: Arc::new(AtomicU64::new(last_seq)),
        persisted_seq: Arc::new(AtomicU64::new(last_seq)),
    })
}

fn cache_warn(message: String) {
    crate::logging::log_warn("cache", message);
}

#[cfg(test)]
mod tests {
    use super::{SEQ_PREFIX_LEN, compare_recent_keys, decode_cached_result, decode_last_seq};
    use crate::cache::memory::CachedImport;

    fn cached_with(result: crate::ipc::protocol::ImportResult, last_seq: u64) -> CachedImport {
        use std::sync::{Arc, atomic::AtomicU64};

        CachedImport {
            result,
            dependency_fingerprints: Vec::new(),
            verified_generation: 0,
            verified_at: None,
            first_party: false,
            last_seq: Arc::new(AtomicU64::new(last_seq)),
            persisted_seq: Arc::new(AtomicU64::new(last_seq)),
        }
    }

    fn sample_cached(last_seq: u64) -> CachedImport {
        let mut result = crate::ipc::protocol::ImportResult::measured(
            "react",
            crate::ipc::protocol::MeasuredSizes {
                raw_bytes: 1,
                minified_bytes: 1,
                gzip_bytes: 1,
                brotli_bytes: 1,
                zstd_bytes: 1,
            },
        );
        result.truly_treeshakeable = true;
        cached_with(result, last_seq)
    }

    fn value_bytes(last_seq: u64) -> Vec<u8> {
        super::encode_cache_value(sample_cached(last_seq)).expect("value should serialize")
    }

    /// **Guard.** The L2 envelope is encoded with `rmp_serde::to_vec` — *positional* msgpack, an
    /// array with no field names. `ImportResult`'s size fields sit in the middle of that array, so
    /// the crate's dominant `Option` idiom, `#[serde(default, skip_serializing_if =
    /// "Option::is_none")]`, would omit them on an Unmeasured result, shorten the array, and every
    /// field after them would decode off by one — measured, not theorised: it fails with
    /// `invalid type: boolean \`false\`, expected u64`. A plain `Option` writes a `nil`
    /// placeholder and keeps the array length.
    ///
    /// This test is the only thing standing between a future contributor "tidying up" those five
    /// attributes and a silently unreadable disk cache for every user who has ever seen a package
    /// the engine could not build.
    #[test]
    fn an_unmeasured_result_round_trips_through_the_positional_disk_encoding() {
        let unmeasured = crate::ipc::protocol::ImportResult::unmeasured(
            "swiper",
            crate::engine::stage::PARSE,
            "unexpected token",
            vec!["entry_path: C:/ws/node_modules/swiper/swiper.mjs".to_owned()],
        );
        let encoded = super::encode_cache_value(cached_with(unmeasured.clone(), 9))
            .expect("an unmeasured result must serialize");

        let decoded = decode_cached_result(&encoded)
            .expect("an unmeasured result must survive the positional msgpack round trip");

        assert_eq!(
            decoded.result.sizes(),
            None,
            "no size went in; none comes out"
        );
        assert_eq!(decoded.result, unmeasured);
    }

    /// **Guard**, and the shape the test above does NOT catch. The five sizes were the *only* fields
    /// protected from the `skip_serializing_if` idiom, but `module_breakdown` and `shared_bytes` sit
    /// mid-struct too — and the daemon really does build this exact pair: an Unmeasured result has
    /// `module_breakdown: None` (skipped, under the old attributes) beside the `shared_bytes:
    /// Some(0)` that `annotate_shared_bytes` stamps on **every** result, measured or not. The
    /// skipped `None` shortened the array, `shared_bytes`'s `0` slid into its slot, and the decode
    /// failed with `invalid type: integer, expected a sequence` — an unreadable disk entry for every
    /// package the engine could not build.
    ///
    /// It was latent only because the annotation runs on the response, one call site away from the
    /// value that is cached. Latent is not fixed.
    #[test]
    fn a_result_with_no_breakdown_but_a_shared_byte_count_round_trips() {
        let mut result = crate::ipc::protocol::ImportResult::unmeasured(
            "swiper",
            crate::engine::stage::LINK,
            "Bundling CSS is no longer supported",
            Vec::new(),
        );
        assert_eq!(result.module_breakdown, None, "the premise: no breakdown");
        result.shared_bytes = Some(0);

        let encoded = super::encode_cache_value(cached_with(result.clone(), 3))
            .expect("the shape must serialize");
        let decoded = decode_cached_result(&encoded).expect(
            "a None field mid-struct must write a nil placeholder, not shorten the array and slide \
             every field after it one slot to the left",
        );

        assert_eq!(decoded.result, result);
        assert_eq!(decoded.result.shared_bytes, Some(0));
    }

    /// **Guard.** `internal_contributions` is `#[serde(skip)]` on `ImportResult` — it is the FULL
    /// module set, far too large for the wire, and only the top 10 go out as `module_breakdown`. The
    /// L2 envelope therefore carries it explicitly as `full_contributions` and restores it on
    /// decode. Nothing pinned that, and the field it protects is now load-bearing twice over:
    /// `annotate_shared_bytes` prefers `internal_contributions` over `module_breakdown`, so if a
    /// cache hit came back without it, every shared-byte figure in the system would silently be
    /// computed from the truncated top-10 list instead of the real graph — a smaller, wrong number,
    /// on exactly the results that hit cache (i.e. almost all of them).
    ///
    /// Delete `full_contributions` from the envelope, or trust `#[serde(skip)]` to round-trip it,
    /// and this goes red.
    #[test]
    fn the_full_module_set_survives_the_l2_round_trip_even_though_the_wire_drops_it() {
        use crate::ipc::protocol::ModuleContribution;

        let mut result = crate::ipc::protocol::ImportResult::measured(
            "react",
            crate::ipc::protocol::MeasuredSizes {
                raw_bytes: 100,
                minified_bytes: 80,
                gzip_bytes: 40,
                brotli_bytes: 30,
                zstd_bytes: 35,
            },
        );
        // The wire carries the top 10; the graph had more, and the extras are what a shared-byte
        // count needs.
        result.internal_contributions = (0..14)
            .map(|index| ModuleContribution {
                path: format!("/workspace/node_modules/react/module-{index:02}.js"),
                bytes: 10 + index,
            })
            .collect();
        result.module_breakdown = Some(result.internal_contributions[..10].to_vec());

        let encoded =
            super::encode_cache_value(cached_with(result.clone(), 5)).expect("encode the envelope");
        let decoded = decode_cached_result(&encoded).expect("decode the envelope");

        assert_eq!(
            decoded.result.internal_contributions, result.internal_contributions,
            "the full module set must come back off disk: `#[serde(skip)]` drops it from the wire, \
             so the L2 envelope has to carry it, and `annotate_shared_bytes` reads it"
        );
        assert_eq!(decoded.result.module_breakdown, result.module_breakdown);
    }

    #[test]
    fn superseded_generation_insert_is_dropped_after_clear() {
        use super::DiskCache;

        // RB-3: the disk resurrection guard. A writer captures the clear generation,
        // then a `clear()` races in and wipes + bumps it. When the pre-clear writer
        // (e.g. `flush_to_disk` replaying a snapshot taken before the clear) finally
        // enqueues its bytes, they carry the STALE generation and must be dropped by the
        // flush — never written back into the cleared shard.
        let dir = std::env::temp_dir().join(format!(
            "il-rb3-gen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = DiskCache::new(Some(dir.clone()), true);
        let cached = sample_cached(1);

        let stale_generation = disk.clear_generation();
        disk.clear(); // wipes and bumps the generation past `stale_generation`

        disk.insert_at_generation("v4:react", &cached, stale_generation)
            .expect("enqueue stale-generation insert");
        disk.flush_pending_inserts();
        assert!(
            disk.get("v4:react").is_none(),
            "a pre-clear (stale-generation) insert must not resurrect a cleared shard (RB-3)"
        );

        // Control: a genuine post-clear insert carrying the CURRENT generation persists.
        let current_generation = disk.clear_generation();
        disk.insert_at_generation("v4:react", &cached, current_generation)
            .expect("enqueue current-generation insert");
        disk.flush_pending_inserts();
        assert!(
            disk.get("v4:react").is_some(),
            "a post-clear insert with the current generation persists as normal"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **The gate is on the READ too** (ADR-0006, invariant 3).
    ///
    /// The write-side gate protects L2 from what it is handed today. It does nothing about a row a
    /// build that predates it already wrote — and L2 outlives the process, so those rows are on real
    /// users' disks right now. Left alone, a `timeout` result would be decoded, served as a cache
    /// hit, and re-promoted into L1 on every access, for as long as the package's bytes did not
    /// change: a transient condition producing a durable wrong answer, which is the one disease this
    /// model exists to end.
    #[test]
    fn a_non_durable_row_already_on_disk_is_refused_on_read_and_evicted() {
        use super::DiskCache;

        let dir = std::env::temp_dir().join(format!(
            "il-l2-hydration-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = DiskCache::new(Some(dir.clone()), true);

        // A MEASURED result whose full-package comparison build timed out: real sizes, a transient
        // diagnostic, and `error: None`. The shape every negative-`error` check waves through, and
        // the one a store must refuse.
        let mut degraded = crate::ipc::protocol::ImportResult::measured(
            "react",
            crate::ipc::protocol::MeasuredSizes {
                raw_bytes: 17_550,
                minified_bytes: 9_000,
                gzip_bytes: 3_000,
                brotli_bytes: 2_500,
                zstd_bytes: 2_400,
            },
        );
        degraded.diagnostics = vec![crate::ipc::protocol::ImportDiagnostic::for_stage(
            crate::engine::stage::TIMEOUT,
            "comparison build did not complete within 8s",
        )];
        assert!(
            !degraded.is_durable(),
            "test setup: this is precisely a result no store may hold"
        );

        // The gate refuses it on the way in, which is why writing it needs the test-only door.
        disk.insert("v4:react:degraded", &cached_with(degraded, 7))
            .expect("the write gate refuses it, and refusing is not an error");
        disk.flush_pending_inserts();
        assert!(
            disk.get("v4:react:degraded").is_none(),
            "premise: the write gate already holds"
        );

        disk.write_ungated_for_test("v4:react:legacy", &cached_with(legacy_degraded(), 7))
            .expect("simulate a row written before the gate existed");
        disk.flush_pending_inserts();

        assert!(
            disk.get("v4:react:legacy").is_none(),
            "a non-durable row already on disk must be refused on READ, not served and re-promoted"
        );
        assert!(
            disk.get("v4:react:legacy").is_none(),
            "and evicted, so the next read does not pay to decode it again"
        );

        // Control: a healthy row written the same way is still served. Without this the fix could be
        // "made to pass" by refusing everything.
        disk.write_ungated_for_test("v4:react:healthy", &sample_cached(8))
            .expect("write a healthy row");
        disk.flush_pending_inserts();
        assert!(
            disk.get("v4:react:healthy").is_some(),
            "a durable row must still hydrate"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreusable_dependency_observation_never_enters_or_leaves_l2() {
        use super::DiskCache;

        let dir = std::env::temp_dir().join(format!(
            "il-l2-unverifiable-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = DiskCache::new(Some(dir.clone()), true);
        let mut cached = sample_cached(9);
        cached.dependency_fingerprints = vec![crate::cache::key::unverifiable_file_fingerprint(
            "/pkg/unreadable.woff2",
        )];

        disk.insert("v4:asset:current", &cached)
            .expect("refusing an unverifiable write is not an error");
        disk.flush_pending_inserts();
        assert!(
            disk.get("v4:asset:current").is_none(),
            "the L2 write boundary must keep no request-local observation"
        );

        disk.write_ungated_for_test("v4:asset:legacy", &cached)
            .expect("simulate a pre-fix row");
        disk.flush_pending_inserts();
        assert!(
            disk.get("v4:asset:legacy").is_none(),
            "the L2 read boundary must evict a pre-fix unverifiable observation"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn legacy_degraded() -> crate::ipc::protocol::ImportResult {
        let mut result = crate::ipc::protocol::ImportResult::measured(
            "react",
            crate::ipc::protocol::MeasuredSizes {
                raw_bytes: 17_550,
                minified_bytes: 9_000,
                gzip_bytes: 3_000,
                brotli_bytes: 2_500,
                zstd_bytes: 2_400,
            },
        );
        result.diagnostics = vec![crate::ipc::protocol::ImportDiagnostic::for_stage(
            crate::engine::stage::TIMEOUT,
            "comparison build did not complete within 8s",
        )];
        result
    }

    #[test]
    fn decode_last_seq_reads_the_prefix_without_full_decode() {
        let value = value_bytes(4242);
        assert_eq!(decode_last_seq(&value), 4242);
        // The prefix IS the first 8 bytes — no envelope parse involved.
        assert_eq!(&value[..SEQ_PREFIX_LEN], 4242_u64.to_le_bytes().as_slice());
    }

    #[test]
    fn decode_last_seq_defaults_short_rows_to_zero() {
        assert_eq!(decode_last_seq(b"short"), 0);
    }

    #[test]
    fn encoded_value_round_trips_through_decode() {
        let value = value_bytes(77);
        let decoded = decode_cached_result(&value).expect("value should decode");
        assert_eq!(
            decoded.last_seq.load(std::sync::atomic::Ordering::Relaxed),
            77
        );
        assert_eq!(decoded.result.specifier, "react");
    }

    #[test]
    fn is_corruption_error_recreates_only_on_genuine_corruption() {
        use super::DiskCache;
        use redb::{DatabaseError, StorageError};
        use std::io::{Error as IoError, ErrorKind};

        // Genuine corruption / unrecoverable on-disk format → wipe + recreate.
        assert!(
            DiskCache::is_corruption_error(&DatabaseError::Storage(StorageError::Corrupted(
                "mangled b-tree".to_owned()
            ))),
            "an explicit Corrupted signal is corruption"
        );
        // redb reports a bad/absent magic number ("not a redb database") as an IO
        // error of kind InvalidData — a format-corruption signal, not a fault.
        assert!(
            DiskCache::is_corruption_error(&DatabaseError::Storage(StorageError::Io(
                IoError::from(ErrorKind::InvalidData)
            ))),
            "a bad magic number (Io/InvalidData) is corruption"
        );
        // A valid file in an older on-disk format with no automatic migration.
        assert!(
            DiskCache::is_corruption_error(&DatabaseError::UpgradeRequired(2)),
            "an un-upgradable old file format is corruption"
        );
        // Needed repair, repair prevented → the shard is unusable as-is.
        assert!(
            DiskCache::is_corruption_error(&DatabaseError::RepairAborted),
            "an aborted repair leaves an unusable shard"
        );

        // Transient / non-corruption → KEEP the (possibly valid) DB. This is the
        // exact X-5 data-loss bug: a lock / AV / permission / flaky-drive IO fault
        // must never be mistaken for corruption.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
            ErrorKind::NotFound,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(
                !DiskCache::is_corruption_error(&DatabaseError::Storage(StorageError::Io(
                    IoError::from(kind)
                ))),
                "transient Io({kind:?}) must be kept, not treated as corruption"
            );
        }
        // A concurrent open is handled before the classifier and is not corruption.
        assert!(!DiskCache::is_corruption_error(
            &DatabaseError::DatabaseAlreadyOpen
        ));
        // Other non-corruption storage / lifecycle states are kept.
        assert!(!DiskCache::is_corruption_error(&DatabaseError::Storage(
            StorageError::PreviousIo
        )));
        assert!(!DiskCache::is_corruption_error(&DatabaseError::Storage(
            StorageError::DatabaseClosed
        )));
        assert!(!DiskCache::is_corruption_error(
            &DatabaseError::TransactionInProgress
        ));
    }

    #[test]
    fn recent_keys_order_is_highest_seq_first_with_key_tiebreak() {
        let mut keys = vec![
            ("b".to_owned(), 10_u64),
            ("a".to_owned(), 30_u64),
            ("c".to_owned(), 30_u64),
        ];
        keys.sort_by(compare_recent_keys);
        // Highest seq first; equal seq breaks by key ascending.
        assert_eq!(
            keys.into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
            vec!["a".to_owned(), "c".to_owned(), "b".to_owned()]
        );
    }
}
