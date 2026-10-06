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

// O(1) per-shard rollup: incrementally maintained byte/count/recency totals, so
// `shard_rollup` reads scalars instead of scanning CACHE_TABLE.
const SUMMARY_TABLE: TableDefinition<&str, u64> = TableDefinition::new("summary");
const SUMMARY_TOTAL_BYTES: &str = "total_bytes";
const SUMMARY_ENTRY_COUNT: &str = "entry_count";
// Recency high-water: the largest `last_seq` ever inserted. Advances on insert,
// is untouched by removals, and is recomputed by the heal-on-open scan. Keeps the
// recency clock ahead of persisted seqs in O(1).
const SUMMARY_MAX_SEQ: &str = "max_seq";
// Secondary index, ascending `(last_seq, key)`: the evictor's lowest-N is a bounded
// range read and `oldest_seq` is the first key. redb compares the `u64` numerically,
// then the key lexicographically, so iteration is ascending by seq.
const SEQ_INDEX_TABLE: TableDefinition<(u64, &str), ()> = TableDefinition::new("seq_index");

// A mismatch recreates the database on open. Bump on any change to a row's layout
// or meaning, including one that still decodes: msgpack reads a plain `17550` into an
// `Option<u64>` size as `Some(17550)`, so a row whose sizes may be fabricated would
// come back as a genuine measurement. SUMMARY_TABLE and SEQ_INDEX_TABLE are written
// in the SAME transaction as every CACHE_TABLE mutation, so a crash cannot tear
// accounting from data.
const CURRENT_SCHEMA_VERSION: u64 = 8;
const INSERT_FLUSH_BATCH: usize = 64;
/// Queue ceiling, reachable only while flushes keep failing: past it the least
/// recently used queued inserts are dropped. They are rebuildable and still live
/// in the memory layer.
const MAX_PENDING_INSERTS: usize = 16 * INSERT_FLUSH_BATCH;
/// After a failed flush, inserts stop triggering flushes for this long; explicit
/// flushes (recycle, shutdown, maintenance reads) still try.
const FLUSH_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// Compact a shard when more than this fraction of its `.redb` file is
/// reclaimable free space (redb reuses freed pages rather than shrinking).
pub const COMPACT_THRESHOLD: f64 = 0.5;
/// A shard is compaction-eligible only after no get or insert for this long.
/// `Database::compact` holds the exclusive lock across the whole rewrite, so
/// compacting an actively analyzed shard would stall its gets. Measured against
/// the coarse `last_access` clock.
const COMPACT_IDLE: Duration = Duration::from_secs(5);

// Every CACHE_TABLE value is `[last_seq: u64 LE, 8 bytes][msgpack CacheEnvelope]`.
// Index and summary maintenance and the heal-on-open rebuild need only `last_seq`
// and the value length, which the fixed prefix gives without deserializing the
// envelope. Recency readers use the `(last_seq, key)` index and the summary.
const SEQ_PREFIX_LEN: usize = 8;

/// redb's page cache defaults to 1 GiB per database and fills with every page touched,
/// so a full scan would leave the whole shard resident. The in-memory `ImportCache` is
/// the hot tier; redb only needs its B-tree interior pages warm.
const REDB_CACHE_BYTES: usize = 8 * 1024 * 1024;

fn create_database(path: &Path) -> Result<Database, redb::DatabaseError> {
    redb::Builder::new()
        .set_cache_size(REDB_CACHE_BYTES)
        .create(path)
}

#[cfg(test)]
#[path = "../../tests/unit/cache_disk_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "../../tests/unit/cache_disk_compaction.rs"]
mod cache_disk_compaction_tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    analyzer_version: String,
    result: ImportResult,
    dependency_fingerprints: Vec<FileFingerprint>,
    full_contributions: Vec<ModuleContribution>,
}

/// A shard's contribution to the global byte budget: its total on-disk bytes, the
/// oldest recency sequence it holds (the victim-selection key), and its entry
/// count. Read in O(1) from the SUMMARY table plus the first key of the
/// `(last_seq, key)` index.
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
    // Behind an RwLock so the compactor can take the exclusive `&mut Database`
    // `Database::compact` requires (which also guarantees no live read transaction),
    // while normal reads and writes share the read lock; redb serializes its own
    // writers.
    db: RwLock<Option<Database>>,
    // Serialized envelopes awaiting a batched commit, as `(clear_generation at
    // enqueue, exact CACHE_TABLE bytes)`. Drained at a size threshold, on
    // recent_keys, on recycle, and on Drop. A flush writes only entries whose tag
    // equals the current generation, so a `clear()` after enqueue drops them.
    pending_inserts: Mutex<HashMap<String, (u64, Vec<u8>)>>,
    // Bumped by `clear()`. A writer captures it before deriving its bytes and tags the
    // queued entry with it.
    clear_generation: AtomicU64,
    // Serializes `clear()` (bump generation, wipe tables, drop pending) against
    // `flush_pending_inserts` (read generation, write kept entries). Off the
    // per-insert hot path.
    clear_lock: Mutex<()>,
    // Coarse wall-clock millis of the last get/insert, read by the compaction idle
    // gate. A heuristic, not a correctness gate, so relaxed ordering is enough.
    last_access: AtomicU64,
    // Unix millis before which an insert must not trigger a flush, set by a failed
    // flush (`FLUSH_RETRY_BACKOFF`); 0 when the last flush succeeded.
    flush_retry_after: AtomicU64,
    // Where an enabled shard lives, kept so a failed open can be retried
    // (`reopen_if_unavailable`). None when the disk cache is disabled.
    storage_path: Option<PathBuf>,
}

impl DiskCache {
    pub fn new(storage_path: Option<PathBuf>, enabled: bool) -> Self {
        Self::open(storage_path, enabled, true)
    }

    /// Opens a shard that must already exist, never creating its directory or
    /// database. For maintenance and observability passes, which open shards from
    /// an earlier directory listing: recreating one that was removed since would
    /// leave a database with no project metadata, which no listing, eviction or
    /// removal ever finds again.
    pub fn open_existing(storage_path: Option<PathBuf>, enabled: bool) -> Self {
        Self::open(storage_path, enabled, false)
    }

    fn open(storage_path: Option<PathBuf>, enabled: bool, create_missing: bool) -> Self {
        if !enabled {
            return Self::disabled();
        }

        let storage_path = match storage_path {
            Some(path) => path,
            None => return Self::disabled(),
        };

        let db = Self::open_database(&storage_path, create_missing);
        Self {
            db: RwLock::new(db),
            storage_path: Some(storage_path),
            pending_inserts: Mutex::new(HashMap::new()),
            clear_generation: AtomicU64::new(0),
            clear_lock: Mutex::new(()),
            // Seeded idle, NOT `now`: a maintenance pass temp-opens unloaded shards
            // and compacts them in the same pass, so a `now` seed would keep every
            // cold shard from ever compacting. A real get/insert stamps `now`.
            last_access: AtomicU64::new(0),
            flush_retry_after: AtomicU64::new(0),
        }
    }

    /// Acquires the shared read lock and returns the guard when a database is
    /// open. Every normal operation borrows `&Database` through this; the guard
    /// must be held for the lifetime of any redb transaction opened from it.
    fn db_read(&self) -> Option<RwLockReadGuard<'_, Option<Database>>> {
        let guard = self.db.read().unwrap_or_else(|poison| poison.into_inner());
        guard.is_some().then_some(guard)
    }

    /// Stamps the shard's last-access clock for the compaction idle gate. Called on
    /// the `get`/`insert` hot paths.
    fn stamp_access(&self) {
        self.last_access.store(
            crate::time::unix_millis_now(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Pushes the last-access clock to the epoch so the shard reads as idle to
    /// `compact_if_fragmented` without sleeping `COMPACT_IDLE`.
    #[cfg(test)]
    pub(crate) fn mark_idle_for_test(&self) {
        self.last_access
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a database is actually open. False when disk caching is disabled
    /// or the open failed (e.g. `DatabaseAlreadyOpen` while a maintenance pass
    /// holds the file).
    pub fn is_available(&self) -> bool {
        self.db_read().is_some()
    }

    /// Reads an entry with the `Freshness` it was served under (`Fresh` or `Unknown`;
    /// `Stale`/`Gone` evict), so a caller mirroring it into memory does not stamp an
    /// `Unknown` entry as verified.
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
        // Read-your-writes: a queued insert not yet flushed is not in the table. Its
        // bytes passed the durability gate on the way in.
        let mut cached = if let Some(pending) = self.pending_insert(key) {
            self.stamp_access();
            pending
        } else {
            self.read_committed(key)?
        };
        // Stamped once at hydration so the per-hit gate never re-decodes the key.
        cached.first_party = crate::cache::key::cache_key_is_first_party(key);
        // Hash-verified per fingerprint, exactly as the memory read does (D18).
        let freshness =
            crate::cache::key::check_fingerprints_strict(&cached.dependency_fingerprints);
        match freshness {
            crate::cache::key::Freshness::Stale | crate::cache::key::Freshness::Gone => {
                self.remove(key);
                None
            }
            // Unknown is transient: keep the entry.
            crate::cache::key::Freshness::Fresh | crate::cache::key::Freshness::Unknown => {
                Some((cached, freshness))
            }
        }
    }

    /// Decodes the committed row for `key`, evicting one that is undecodable or not
    /// durable.
    fn read_committed(&self, key: &str) -> Option<CachedImport> {
        // Scope the read guard: `remove` re-acquires the db lock, and a re-entrant
        // read while a compaction writer is queued deadlocks (std `RwLock` blocks
        // new readers behind a queued writer). Decide inside, drop the guard, THEN remove.
        let decoded = {
            let db_guard = self.db_read()?;
            // Stamp only once the DB is confirmed open, and before the table read so a
            // miss still counts as shard activity.
            self.stamp_access();
            let db = db_guard.as_ref().expect("db present under read guard");
            let read_txn = db.begin_read().ok()?;
            let table = read_txn.open_table(CACHE_TABLE).ok()?;
            let value = table.get(key).ok()??;
            decode_cached_result(value.value())
        };

        let Some(cached) = decoded else {
            // Undecodable row (corrupt or written by an incompatible build).
            self.remove(key);
            return None;
        };
        // The durability gate is on the READ too (ADR-0006, invariant 3): L2 outlives
        // the process, so a non-durable row on disk would otherwise be served and
        // re-promoted into L1 forever. Refusing it costs one rebuild.
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
        Some(cached)
    }

    /// The current clear generation. A writer captures this BEFORE deriving the bytes
    /// it will queue; if a `clear()` bumps it in between, `flush_pending_inserts` drops
    /// those now-stale bytes so a wipe cannot be undone by an in-flight writer.
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
    /// `generation`. Writers that derive bytes from an earlier memory snapshot capture
    /// the generation before it, so a `clear()` in between drops this entry at flush
    /// instead of resurrecting the wiped shard.
    ///
    /// **The transience gate is applied here too**, not only in `ImportCache` (ADR-0006,
    /// invariant 3): L2 is a store in its own right and outlives the process. A refused insert
    /// is a no-op, not an error: `Err` marks the key dirty for a flush replay, which would
    /// defeat the refusal.
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

    /// The write with the durability gate skipped, so a test can plant a non-durable row (as an
    /// older build may have left on disk) and exercise the read-side gate. Test-only.
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
        self.stamp_access();

        let mut persisted = cached.clone();
        persisted.result.cache_hit = false;

        let bytes = encode_cache_value(persisted)?;

        // Queued for a batched commit: one durable transaction per entry would
        // serialize N fsyncs on redb's single writer.
        let should_flush = match self.pending_inserts.lock() {
            Ok(mut pending) => {
                pending.insert(key.to_owned(), (generation, bytes));
                shed_oldest_pending_inserts(&mut pending);
                pending.len() >= INSERT_FLUSH_BATCH
                    && crate::time::unix_millis_now()
                        >= self
                            .flush_retry_after
                            .load(std::sync::atomic::Ordering::Relaxed)
            }
            Err(_) => return Err("cache pending-insert lock poisoned".to_owned()),
        };
        if should_flush {
            self.flush_pending_inserts();
        }
        Ok(())
    }

    pub fn flush_pending_inserts(&self) {
        // Serialized against `clear()`: this flush runs entirely before a clear (its
        // writes are then wiped) or entirely after (it sees the bumped generation and
        // drops every pre-clear entry). A poisoned lock is recovered so a prior panic
        // cannot wedge all future flushes.
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

        // Only bytes carrying the current generation are written. Read under
        // `clear_lock`, so the generation cannot change before the write.
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

        match write_pending_inserts(db, &kept) {
            Ok(()) => self
                .flush_retry_after
                .store(0, std::sync::atomic::Ordering::Relaxed),
            Err(error) => {
                self.flush_retry_after.store(
                    crate::time::unix_millis_now()
                        .saturating_add(FLUSH_RETRY_BACKOFF.as_millis() as u64),
                    std::sync::atomic::Ordering::Relaxed,
                );
                if let Ok(mut current) = self.pending_inserts.lock() {
                    // Re-queue the entries we tried to write with their generation tag.
                    for (key, bytes) in kept {
                        current.entry(key).or_insert((generation, bytes));
                    }
                    shed_oldest_pending_inserts(&mut current);
                }
                cache_warn(format!("failed to flush cache inserts: {error}"));
            }
        }
    }

    /// The queued-but-unflushed entry for `key`, served only while its generation
    /// still matches, so a read cannot undo a `clear()` that superseded it.
    fn pending_insert(&self, key: &str) -> Option<CachedImport> {
        let bytes = {
            let pending = self.pending_inserts.lock().ok()?;
            let (entry_generation, bytes) = pending.get(key)?;
            if *entry_generation != self.clear_generation() {
                return None;
            }
            bytes.clone()
        };
        decode_cached_result(&bytes)
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

    /// Returns the `limit` most-recently-used keys, highest `last_seq` first, ties
    /// by key ascending. A reverse range read of the `(last_seq, key)` index, so a
    /// cold open's preload costs O(limit) rows, not a scan of the shard.
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
        let seq_index = match read_txn.open_table(SEQ_INDEX_TABLE) {
            Ok(seq_index) => seq_index,
            Err(error) => {
                cache_warn(format!("failed to open seq index for recent keys: {error}"));
                return Vec::new();
            }
        };
        let iter = match seq_index.iter() {
            Ok(iter) => iter,
            Err(error) => {
                cache_warn(format!("failed to iterate seq index: {error}"));
                return Vec::new();
            }
        };
        // Walk the index from the highest seq down. Equal seqs come key-descending
        // in reverse, so keep every row tied with the last one taken and let the sort
        // pick the same `limit` keys a full sort would.
        let mut keys: Vec<(String, u64)> = Vec::with_capacity(limit);
        for entry in iter.rev() {
            let Ok((row, _)) = entry else { continue };
            let (seq, key) = row.value();
            if keys.len() >= limit && keys.last().is_some_and(|(_, last)| *last != seq) {
                break;
            }
            keys.push((key.to_owned(), seq));
        }
        keys.sort_by(compare_recent_keys);
        keys.truncate(limit);
        keys.into_iter().map(|(key, _)| key).collect()
    }

    /// Summary of this shard for the byte-budget coordinator: total on-disk bytes
    /// (summed CACHE_TABLE value lengths), the oldest recency sequence held, and the
    /// entry count, read from the SUMMARY table and the seq index. Also advances the
    /// recency clock past every persisted seq so a post-restart access sorts newer
    /// than durable entries.
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
        // Keep the live recency clock ahead of every persisted seq.
        crate::cache::recency::RecencyClock::observe(read_summary_field(&summary, SUMMARY_MAX_SEQ));

        if entry_count == 0 {
            return ShardRollup::empty();
        }

        // `oldest_seq` is the first key of the ascending `(last_seq, key)` index.
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

    /// The largest `last_seq` persisted in this shard, a single-key read of the
    /// SUMMARY `max_seq` high-water. The startup recency seed uses it to lift the
    /// process-global clock above every persisted seq before serving. Returns `0` for
    /// an empty shard or an unavailable database. Flushes queued inserts first so a
    /// not-yet-committed high seq is included.
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

    /// Returns up to `n` of the shard's lowest-`last_seq` keys with their persisted
    /// seq, excluding the shard's `floor` highest-seq entries (the per-project floor).
    /// Empty when every entry is within the floor. The persisted seq lets a caller
    /// with a memory layer shield entries promoted since their last persist.
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

        // `take` bounds the range read to what the evictor asked for, past the floor.
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

        // The index is ascending by `(last_seq, key)`, so the first `take` rows are
        // the lowest-seq keys beyond the floor.
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
        // Freed bytes are the summed removed value lengths, matching the rollup
        // accounting; only bytes the commit durably freed are reported.
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

    /// Evicts every entry belonging to any package in `package_names` in a single
    /// table scan that decodes each key once.
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
            // Drop the table handle before `maintain_removals` re-opens CACHE_TABLE:
            // redb forbids opening the same table twice in one write transaction.
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

    /// Reclaims redb free pages when the shard is idle and its free-space ratio
    /// exceeds `threshold`. redb reuses freed pages rather than shrinking the file, so
    /// after heavy eviction the file can far exceed the logical byte budget.
    ///
    /// Gated in two stages: a shard touched within `COMPACT_IDLE` is skipped, then the
    /// fragmentation ratio is probed under the shared read guard so a non-fragmented
    /// shard never takes the exclusive lock. Only then does it take `db.write()`:
    /// `Database::compact` needs `&mut Database` and fails if any read transaction is
    /// live. Returns whether it compacted.
    pub fn compact_if_fragmented(&self, threshold: f64) -> bool {
        let idle_for = Duration::from_millis(
            crate::time::unix_millis_now()
                .saturating_sub(self.last_access.load(std::sync::atomic::Ordering::Relaxed)),
        );
        if idle_for < COMPACT_IDLE {
            return false;
        }

        // Drop the shared guard before escalating: std `RwLock` cannot upgrade a
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

        let mut guard = self.db.write().unwrap_or_else(|poison| poison.into_inner());
        let Some(database) = guard.as_mut() else {
            return false;
        };

        // Recompute under the exclusive guard: a concurrent insert or evict may have
        // moved the ratio since the probe.
        let free_ratio = fragmentation_ratio(database);
        if free_ratio <= threshold {
            return false;
        }

        // Logged so the time spent holding the exclusive lock stays observable.
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
        // Bump the clear generation FIRST, under `clear_lock`, so any writer that
        // captured the old one is superseded: its queued bytes fail the flush filter and
        // the ImportCache memory-rollback guard sees the change. Bumped even with disk
        // disabled, since memory-only mode still relies on it. A poisoned lock is
        // recovered so a prior panic cannot wedge every future clear.
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
            // Empty the cache and index tables and zero the summary.
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

    /// Rebuilds the summary and index when the persisted `entry_count` disagrees with
    /// the CACHE_TABLE row count. The check is O(1); the O(N) rebuild runs only on
    /// drift or an absent summary.
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
            last_access: AtomicU64::new(0),
            flush_retry_after: AtomicU64::new(0),
            storage_path: None,
        }
    }

    /// Retries the open of an enabled shard whose database is not open, creating it
    /// if missing. Returns whether a database is open afterwards.
    pub fn reopen_if_unavailable(&self) -> bool {
        if self.is_available() {
            return true;
        }
        let Some(storage_path) = self.storage_path.as_ref() else {
            return false;
        };
        // Opened outside the lock so readers are not stalled on the open's I/O.
        let Some(opened) = Self::open_database(storage_path, true) else {
            return false;
        };
        let mut guard = self.db.write().unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            *guard = Some(opened);
        }
        true
    }

    fn open_database(storage_path: &Path, create_missing: bool) -> Option<Database> {
        if !create_missing {
            if !storage_path.join(CACHE_DB_FILE_NAME).is_file() {
                return None;
            }
        } else if let Err(error) = fs::create_dir_all(storage_path) {
            cache_warn(format!(
                "failed to create cache directory {}: {error}",
                storage_path.display()
            ));
            return None;
        }

        let db_path = storage_path.join(CACHE_DB_FILE_NAME);
        let db_existed = db_path.exists();
        let db = match create_database(&db_path) {
            Ok(db) => db,
            // Already open elsewhere in this process (redb allows one Database per
            // file), e.g. a maintenance temp open racing the shard being loaded.
            // NEVER recreate here: `recreate_database` unlinks the live shard's file.
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
                // Only genuine corruption justifies unlinking the shard. A transient
                // failure (sharing violation, AV lock, permission, offline drive) keeps
                // the possibly-valid file so a later open retries.
                if Self::is_corruption_error(&error) {
                    return Self::recreate_database(&db_path);
                }
                return None;
            }
        };

        match Self::ensure_schema(&db, !db_existed) {
            Ok(()) => {
                // O(1) drift check on open; rebuilds from a scan only on mismatch.
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
            // A transient schema-read failure keeps the possibly-valid DB so a later
            // open retries.
            Err(SchemaError::Transient(message)) => {
                cache_warn(format!(
                    "cache database {} schema check failed transiently, keeping it: {message}",
                    db_path.display()
                ));
                None
            }
        }
    }

    /// True only for genuine on-disk corruption or an unrecoverable format, which
    /// justifies wiping and recreating the shard. A transient open failure (lock, AV,
    /// permission, IO on an offline drive) keeps the possibly-valid DB.
    ///
    /// redb's `DatabaseError` / `StorageError` are `#[non_exhaustive]`; the catch-all
    /// keeps every unclassified error, including future variants. Never `_ => true`.
    pub(crate) fn is_corruption_error(error: &redb::DatabaseError) -> bool {
        use redb::{DatabaseError, StorageError};
        match error {
            // redb detected a corrupted on-disk structure it could not recover.
            DatabaseError::Storage(StorageError::Corrupted(_)) => true,
            // A valid file in an older on-disk format redb can no longer open, with
            // no automatic migration. For a rebuildable cache the sanctioned
            // recovery is the same wipe-and-recreate as a schema-version mismatch.
            DatabaseError::UpgradeRequired(_) => true,
            // Repair did not complete (defensive: reachable only via an aborting
            // repair callback or a read-only open, neither of which
            // `Database::create` installs). Never a transient fault, so recreate.
            DatabaseError::RepairAborted => true,
            // A bad or absent magic number surfaces as IO `InvalidData`, the only
            // `InvalidData` redb produces while opening. Transient faults surface
            // under other kinds, which are kept.
            DatabaseError::Storage(StorageError::Io(source)) => {
                source.kind() == std::io::ErrorKind::InvalidData
            }
            // Not positively corruption, so KEEP the possibly-valid database:
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

        let db = match create_database(db_path) {
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
                // An existing database with no version key is incompatible, not a
                // transient fault: the migration wipe recreates it.
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
            // Create the accounting tables so maintenance paths never race a
            // missing-table open.
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
/// version. The dispositions differ: recreate vs. keep.
enum SchemaError {
    /// The stored version differs from `CURRENT_SCHEMA_VERSION`, or an existing
    /// database has no version key. The shard is recreated empty.
    Incompatible(String),
    /// A failure while reading or writing the schema. The database may be valid, so
    /// it is kept and a later open retries.
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

/// The shard's reclaimable-free-space ratio (fragmented / allocated bytes). redb
/// exposes it only via `WriteTransaction::stats()`, but `begin_write` needs just
/// `&Database`, so this runs under the caller's shared guard; the throwaway
/// transaction is aborted. Returns `0.0` on any error so a probe failure never
/// provokes a compact.
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
    #[cfg(test)]
    if test_support::should_fail_flush(pending.keys()) {
        return Err("forced cache flush failure".to_owned());
    }
    let write_txn = db
        .begin_write()
        .map_err(|error| format!("failed to begin cache write: {error}"))?;

    let summary_consistent = {
        let mut cache = write_txn
            .open_table(CACHE_TABLE)
            .map_err(|error| format!("failed to open cache table: {error}"))?;
        let mut summary = write_txn
            .open_table(SUMMARY_TABLE)
            .map_err(|error| format!("failed to open summary table: {error}"))?;
        let mut seq_index = write_txn
            .open_table(SEQ_INDEX_TABLE)
            .map_err(|error| format!("failed to open seq index table: {error}"))?;

        // Deltas fold into locals and the summary is written once; i128 so a
        // replace with a smaller value goes negative instead of wrapping.
        let mut total_bytes = read_summary_field(&summary, SUMMARY_TOTAL_BYTES) as i128;
        let mut entry_count = read_summary_field(&summary, SUMMARY_ENTRY_COUNT) as i128;
        let mut max_seq = read_summary_field(&summary, SUMMARY_MAX_SEQ);

        for (key, bytes) in pending {
            let new_len = bytes.len() as u64;
            let new_seq = decode_last_seq(bytes);

            // `insert` returns the prior value: its seq and length drive the index
            // and byte maintenance.
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

        write_summary(&mut summary, total_bytes, entry_count, Some(max_seq))?
    };
    if !summary_consistent {
        rebuild_summary_in_txn(&write_txn)?;
    }

    write_txn
        .commit()
        .map_err(|error| format!("failed to commit cache write: {error}"))
}

/// Drops the least recently used queued inserts once the queue passes
/// `MAX_PENDING_INSERTS`, which only failing flushes allow. Sheds a batch at a time
/// so a queue pinned at the ceiling does not sort on every insert.
fn shed_oldest_pending_inserts(pending: &mut HashMap<String, (u64, Vec<u8>)>) {
    if pending.len() <= MAX_PENDING_INSERTS {
        return;
    }
    let mut by_recency = pending
        .iter()
        .map(|(key, (_, bytes))| (decode_last_seq(bytes), key.clone()))
        .collect::<Vec<_>>();
    by_recency.sort_unstable();
    let excess = pending.len() - (MAX_PENDING_INSERTS - INSERT_FLUSH_BATCH);
    for (_, key) in by_recency.into_iter().take(excess) {
        pending.remove(&key);
    }
}

/// Reads a `u64` summary field, defaulting to `0` when the key is absent (a fresh
/// shard has no summary rows until its first insert).
fn read_summary_field<T: ReadableTable<&'static str, u64>>(table: &T, field: &str) -> u64 {
    table
        .get(field)
        .ok()
        .flatten()
        .map(|value| value.value())
        .unwrap_or(0)
}

/// Writes `total_bytes`/`entry_count` back to SUMMARY, and `max_seq` when supplied.
/// Removals pass `None`: the high-water mark only advances on insert.
///
/// Every delta is exact against the stored row it replaces or removes, so a negative
/// result means the summary had already drifted. That writes nothing and returns
/// `false`: the caller rebuilds from a scan in the same transaction, because a clamp
/// to zero would keep the drift and hide it from the count-only heal check.
fn write_summary(
    summary: &mut redb::Table<'_, &'static str, u64>,
    total_bytes: i128,
    entry_count: i128,
    max_seq: Option<u64>,
) -> Result<bool, String> {
    let (Ok(total_bytes), Ok(entry_count)) =
        (u64::try_from(total_bytes), u64::try_from(entry_count))
    else {
        return Ok(false);
    };
    summary
        .insert(SUMMARY_TOTAL_BYTES, total_bytes)
        .map_err(|error| format!("failed to write summary total bytes: {error}"))?;
    summary
        .insert(SUMMARY_ENTRY_COUNT, entry_count)
        .map_err(|error| format!("failed to write summary entry count: {error}"))?;
    if let Some(max_seq) = max_seq {
        summary
            .insert(SUMMARY_MAX_SEQ, max_seq)
            .map_err(|error| format!("failed to write summary max seq: {error}"))?;
    }
    Ok(true)
}

/// Removes `keys` from CACHE_TABLE inside `write_txn`, maintaining SUMMARY and
/// SEQ_INDEX in the SAME transaction, and returns the total on-disk bytes freed.
/// The caller owns the commit, so an aborted txn leaves data and accounting
/// consistent. `max_seq` is a high-water mark and is left untouched.
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

    let summary_consistent = write_summary(&mut summary, total_bytes, entry_count, None)?;
    drop((cache, summary, seq_index));
    if !summary_consistent {
        rebuild_summary_in_txn(write_txn)?;
    }
    Ok(freed)
}

/// Recomputes SUMMARY and rebuilds SEQ_INDEX from a full CACHE_TABLE scan inside
/// `write_txn` (caller commits): the heal fallback if incremental maintenance
/// ever misses a site.
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
    .map(|_| ())
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

    /// **Guard.** The L2 envelope is *positional* msgpack (`rmp_serde::to_vec`, an array with no
    /// field names). `ImportResult`'s size fields sit mid-array, so
    /// `#[serde(skip_serializing_if = "Option::is_none")]` on them would shorten the array on an
    /// Unmeasured result and every later field would decode off by one. A plain `Option` writes a
    /// `nil` placeholder.
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

    /// **Guard**, for the same positional-encoding rule on other mid-struct `Option`s. An
    /// Unmeasured result has `module_breakdown: None` beside the `shared_bytes: Some(0)` that
    /// `annotate_shared_bytes` stamps on every result; skipping the `None` would slide the `0`
    /// into its slot and make the row undecodable.
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

    /// **Guard.** `internal_contributions` is `#[serde(skip)]` on `ImportResult` (the full module
    /// set; only the top 10 go out as `module_breakdown`), so the L2 envelope carries it as
    /// `full_contributions`. `annotate_shared_bytes` prefers it, so a cache hit without it would
    /// compute every shared-byte figure from the truncated top 10: a smaller, wrong number.
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
        // The wire carries the top 10; a shared-byte count needs the rest.
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

        // A writer captures the clear generation, then a `clear()` wipes and bumps it.
        // The writer's later enqueue carries the stale generation and is dropped by the
        // flush, never written back into the cleared shard.
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
            disk.get_with_freshness("v4:react")
                .map(|(cached, _)| cached)
                .is_none(),
            "a pre-clear (stale-generation) insert must not resurrect a cleared shard (RB-3)"
        );

        // Control: a genuine post-clear insert carrying the CURRENT generation persists.
        let current_generation = disk.clear_generation();
        disk.insert_at_generation("v4:react", &cached, current_generation)
            .expect("enqueue current-generation insert");
        disk.flush_pending_inserts();
        assert!(
            disk.get_with_freshness("v4:react")
                .map(|(cached, _)| cached)
                .is_some(),
            "a post-clear insert with the current generation persists as normal"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **The gate is on the READ too** (ADR-0006, invariant 3). L2 outlives the process, so a
    /// non-durable row already on disk would otherwise be served and re-promoted into L1 on every
    /// access: a transient condition producing a durable wrong answer.
    #[test]
    fn a_non_durable_row_already_on_disk_is_refused_on_read_and_evicted() {
        use super::DiskCache;

        let dir = std::env::temp_dir().join(format!(
            "il-l2-hydration-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = DiskCache::new(Some(dir.clone()), true);

        // A measured result whose comparison build timed out: real sizes, a transient diagnostic,
        // and `error: None`. A store must refuse it.
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

        // The write gate refuses it, so planting it needs the test-only write.
        disk.insert("v4:react:degraded", &cached_with(degraded, 7))
            .expect("the write gate refuses it, and refusing is not an error");
        disk.flush_pending_inserts();
        assert!(
            disk.get_with_freshness("v4:react:degraded")
                .map(|(cached, _)| cached)
                .is_none(),
            "premise: the write gate already holds"
        );

        disk.write_ungated_for_test("v4:react:legacy", &cached_with(legacy_degraded(), 7))
            .expect("simulate a row written before the gate existed");
        disk.flush_pending_inserts();

        assert!(
            disk.get_with_freshness("v4:react:legacy")
                .map(|(cached, _)| cached)
                .is_none(),
            "a non-durable row already on disk must be refused on READ, not served and re-promoted"
        );
        assert!(
            disk.get_with_freshness("v4:react:legacy")
                .map(|(cached, _)| cached)
                .is_none(),
            "and evicted, so the next read does not pay to decode it again"
        );

        // Control: a healthy row written the same way is still served.
        disk.write_ungated_for_test("v4:react:healthy", &sample_cached(8))
            .expect("write a healthy row");
        disk.flush_pending_inserts();
        assert!(
            disk.get_with_freshness("v4:react:healthy")
                .map(|(cached, _)| cached)
                .is_some(),
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
            disk.get_with_freshness("v4:asset:current")
                .map(|(cached, _)| cached)
                .is_none(),
            "the L2 write boundary must keep no request-local observation"
        );

        disk.write_ungated_for_test("v4:asset:legacy", &cached)
            .expect("simulate a pre-fix row");
        disk.flush_pending_inserts();
        assert!(
            disk.get_with_freshness("v4:asset:legacy")
                .map(|(cached, _)| cached)
                .is_none(),
            "the L2 read boundary must evict a pre-fix unverifiable observation"
        );

        drop(disk);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// On a full disk or a read-only remount every flush fails. Inserts must back off rather than
    /// retry the whole backlog each time, and the queue must stay bounded.
    #[test]
    fn a_failing_disk_neither_retries_every_insert_nor_grows_the_queue_without_bound() {
        use super::{DiskCache, MAX_PENDING_INSERTS, test_support};

        let dir = std::env::temp_dir().join(format!(
            "il-flush-backoff-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let disk = DiskCache::new(Some(dir.clone()), true);
        let token = test_support::unique_failure_token("flush-backoff");
        test_support::fail_flushes_containing(&token);
        let key = |index: usize| format!("v4:{token}:{index}");

        let inserted = MAX_PENDING_INSERTS * 2;
        for index in 0..inserted {
            disk.insert(&key(index), &sample_cached(index as u64 + 1))
                .expect("a queued insert is not an error");
        }

        assert_eq!(
            test_support::take_flush_attempts_for_token(&token),
            1,
            "one failed flush, then inserts back off instead of retrying the backlog"
        );
        assert!(disk.pending_inserts.lock().unwrap().len() <= MAX_PENDING_INSERTS);
        assert!(
            disk.get_with_freshness(&key(inserted - 1))
                .map(|(cached, _)| cached)
                .is_some(),
            "the newest entries stay queued"
        );
        assert!(
            disk.get_with_freshness(&key(0))
                .map(|(cached, _)| cached)
                .is_none(),
            "the least recently used are shed"
        );

        test_support::stop_failing_flushes_containing(&token);
        disk.flush_pending_inserts();
        assert!(disk.pending_inserts.lock().unwrap().is_empty());
        assert!(
            disk.get_with_freshness(&key(inserted - 1))
                .map(|(cached, _)| cached)
                .is_some(),
            "a recovered disk persists the queue"
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
        // The prefix is the first 8 bytes; no envelope parse involved.
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

        // Genuine corruption or unrecoverable on-disk format: wipe and recreate.
        assert!(
            DiskCache::is_corruption_error(&DatabaseError::Storage(StorageError::Corrupted(
                "mangled b-tree".to_owned()
            ))),
            "an explicit Corrupted signal is corruption"
        );
        // redb reports a bad or absent magic number as IO InvalidData.
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
        // Repair needed but prevented: the shard is unusable as-is.
        assert!(
            DiskCache::is_corruption_error(&DatabaseError::RepairAborted),
            "an aborted repair leaves an unusable shard"
        );

        // Transient faults (lock, AV, permission, flaky drive) keep the possibly-valid DB.
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
