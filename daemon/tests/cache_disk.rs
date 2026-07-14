use import_lens_daemon::{
    cache::{disk::DiskCache, memory::CachedImport, memory::ImportCache},
    ipc::protocol::{ConfidenceLevel, ImportDiagnostic, ImportResult},
};
use redb::{Database, ReadableDatabase, TableDefinition};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 8;

mod common;

fn temp_storage() -> PathBuf {
    common::temp_workspace("import-lens-cache")
}

fn db_path(storage_path: &Path) -> PathBuf {
    storage_path.join("importlens.redb")
}

#[test]
fn opening_a_stale_schema_db_recreates_it_empty() {
    use redb::ReadableTableMetadata;
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Simulate a cache written by a prior schema version (one below current) with
    // a junk row, using the same table defs the daemon uses.
    {
        let db = Database::create(db_path(&storage)).expect("create db");
        let write = db.begin_write().expect("begin");
        {
            let mut meta = write.open_table(METADATA_TABLE).expect("meta table");
            meta.insert(SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION - 1)
                .expect("write old version");
            let mut cache = write.open_table(CACHE_TABLE).expect("cache table");
            cache
                .insert("v3:stale", b"junk".as_slice())
                .expect("write stale row");
        }
        write.commit().expect("commit");
    }

    // Opening through the daemon must detect the mismatch and recreate empty.
    let cache = ImportCache::new(Some(storage.clone()), true);
    drop(cache);

    let db = Database::open(db_path(&storage)).expect("reopen");
    let read = db.begin_read().expect("read");
    let table = read.open_table(CACHE_TABLE).expect("cache table");
    assert_eq!(
        table.len().expect("len"),
        0,
        "a stale-schema cache should be wiped on open"
    );

    fs::remove_dir_all(storage).expect("cleanup");
}

fn result(specifier: &str) -> ImportResult {
    let mut result = ImportResult::measured(
        specifier,
        import_lens_daemon::ipc::protocol::MeasuredSizes {
            raw_bytes: 10,
            minified_bytes: 8,
            gzip_bytes: 7,
            brotli_bytes: 6,
            zstd_bytes: 5,
        },
    );
    result.truly_treeshakeable = true;
    result.confidence = ConfidenceLevel::High;
    result.confidence_reasons = vec!["test fixture confidence".to_owned()];
    result.diagnostics = vec![ImportDiagnostic {
        // A real informational stage. A fabricated one ("test") is now REFUSED by every durable
        // store — an unclassified stage is not durable (`pipeline::stage`) — so a fixture that used
        // one was building a result the cache correctly declines to keep.
        stage: import_lens_daemon::engine::diagnostic_stage::EXTERNAL.to_owned(),
        message: "cached".to_owned(),
        details: Vec::new(),
    }];
    result
}

fn read_schema_version(storage_path: &Path) -> u64 {
    let db = Database::open(db_path(storage_path)).expect("cache database should open");
    let read_txn = db.begin_read().expect("read transaction should begin");
    let table = read_txn
        .open_table(METADATA_TABLE)
        .expect("metadata table should exist");

    table
        .get(SCHEMA_VERSION_KEY)
        .expect("schema version should be readable")
        .expect("schema version should exist")
        .value()
}

fn write_database_with_schema(storage_path: &Path, schema_version: u64) {
    let db = Database::create(db_path(storage_path)).expect("cache database should be created");
    let write_txn = db.begin_write().expect("write transaction should begin");

    {
        let mut metadata = write_txn
            .open_table(METADATA_TABLE)
            .expect("metadata table should open");
        metadata
            .insert(SCHEMA_VERSION_KEY, schema_version)
            .expect("schema version should be written");
    }

    {
        let bytes = rmp_serde::to_vec(&result("react")).expect("result should serialize");
        let mut cache = write_txn
            .open_table(CACHE_TABLE)
            .expect("cache table should open");
        cache
            .insert("react@18.3.1::default", bytes.as_slice())
            .expect("cache entry should be written");
    }

    write_txn.commit().expect("database should commit");
}

fn write_database_without_schema_version(storage_path: &Path) {
    let db = Database::create(db_path(storage_path)).expect("cache database should be created");
    let write_txn = db.begin_write().expect("write transaction should begin");

    {
        write_txn
            .open_table(METADATA_TABLE)
            .expect("metadata table should open");
    }

    {
        let bytes = rmp_serde::to_vec(&result("react")).expect("result should serialize");
        let mut cache = write_txn
            .open_table(CACHE_TABLE)
            .expect("cache table should open");
        cache
            .insert("react@18.3.1::default", bytes.as_slice())
            .expect("cache entry should be written");
    }

    write_txn.commit().expect("database should commit");
}

fn write_corrupt_cache_entry(storage_path: &Path, key: &str) {
    let db = Database::create(db_path(storage_path)).expect("cache database should be created");
    let write_txn = db.begin_write().expect("write transaction should begin");

    {
        let mut metadata = write_txn
            .open_table(METADATA_TABLE)
            .expect("metadata table should open");
        metadata
            .insert(SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION)
            .expect("schema version should be written");
    }

    {
        let mut cache = write_txn
            .open_table(CACHE_TABLE)
            .expect("cache table should open");
        cache
            .insert(key, b"not-messagepack".as_slice())
            .expect("corrupt cache entry should be written");
    }

    write_txn.commit().expect("database should commit");
}

#[test]
fn disk_cache_creates_versioned_metadata_table() {
    let storage_path = temp_storage();

    let cache = ImportCache::new(Some(storage_path.clone()), true);
    drop(cache);

    assert_eq!(read_schema_version(&storage_path), CURRENT_SCHEMA_VERSION);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_preloads_entries_into_memory_on_startup() {
    let storage_path = temp_storage();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
    }

    let cache = ImportCache::new(Some(storage_path.clone()), true);

    assert_eq!(cache.memory_len(), 1);
    assert!(
        cache
            .get("react@18.3.1::default")
            .expect("cache entry should be preloaded")
            .cache_hit
    );
    drop(cache);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_preloads_at_most_recent_entry_limit() {
    let storage_path = temp_storage();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        // Insert order fixes recency: each insert stamps a strictly higher seq.
        for index in 0..5 {
            cache.insert(format!("pkg-{index}@1.0.0::default"), result("pkg"));
        }
    }

    let cache = ImportCache::new_with_recent_preload_limit(Some(storage_path.clone()), true, 2);

    assert_eq!(cache.memory_len(), 2);
    drop(cache);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_lazy_hit_populates_memory_cache() {
    let storage_path = temp_storage();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
    }

    let cache = ImportCache::new_with_recent_preload_limit(Some(storage_path.clone()), true, 0);

    assert_eq!(cache.memory_len(), 0);
    assert!(
        cache
            .get("react@18.3.1::default")
            .expect("lazy disk hit should return result")
            .cache_hit
    );
    assert_eq!(cache.memory_len(), 1);
    drop(cache);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn recent_keys_returns_highest_last_seq_first() {
    let storage_path = temp_storage();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        // Insert in order: each insert stamps a strictly higher last_seq, so the
        // most-recently-inserted keys are the most recent.
        cache.insert("a@1.0.0::default".to_owned(), result("a"));
        cache.insert("b@1.0.0::default".to_owned(), result("b"));
        cache.insert("c@1.0.0::default".to_owned(), result("c"));
        cache.flush_to_disk().expect("flush should succeed");

        assert_eq!(
            cache.recent_keys(2),
            vec!["c@1.0.0::default".to_owned(), "b@1.0.0::default".to_owned()],
            "recent_keys returns the highest-last_seq keys, most-recent first"
        );
    }

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn removing_an_entry_leaves_no_orphan_recency() {
    let storage_path = temp_storage();

    let cache = ImportCache::new(Some(storage_path.clone()), true);
    cache.insert("react@18.3.1::default".to_owned(), result("react"));
    cache.insert("vue@3.0.0::default".to_owned(), result("vue"));
    cache.flush_to_disk().expect("flush should succeed");

    // Recency lives inside the entry, so invalidating the entry removes its recency
    // with no separate recents row to dangle.
    cache.invalidate_package("react");

    assert_eq!(
        cache.recent_keys(10),
        vec!["vue@3.0.0::default".to_owned()],
        "an invalidated entry must not linger in the recency ordering"
    );

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_skips_corrupt_entries_without_poisoning_memory() {
    let storage_path = temp_storage();
    let key = "corrupt@1.0.0::default";
    write_corrupt_cache_entry(&storage_path, key);

    let cache = ImportCache::new_with_recent_preload_limit(Some(storage_path.clone()), true, 1);

    assert_eq!(cache.memory_len(), 0);
    assert!(cache.get(key).is_none());
    assert_eq!(cache.memory_len(), 0);
    drop(cache);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_recreates_database_when_schema_mismatches() {
    let storage_path = temp_storage();
    write_database_with_schema(&storage_path, 999);

    let cache = ImportCache::new(Some(storage_path.clone()), true);

    assert_eq!(cache.memory_len(), 0);
    assert!(cache.get("react@18.3.1::default").is_none());
    drop(cache);

    assert_eq!(read_schema_version(&storage_path), CURRENT_SCHEMA_VERSION);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_recreates_existing_database_when_schema_version_is_missing() {
    let storage_path = temp_storage();
    write_database_without_schema_version(&storage_path);

    let cache = ImportCache::new(Some(storage_path.clone()), true);

    assert_eq!(cache.memory_len(), 0);
    assert!(cache.get("react@18.3.1::default").is_none());
    drop(cache);

    assert_eq!(read_schema_version(&storage_path), CURRENT_SCHEMA_VERSION);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_recovers_from_corrupt_database_file() {
    let storage_path = temp_storage();
    fs::write(db_path(&storage_path), b"not a redb database")
        .expect("corrupt database should be written");

    let cache = ImportCache::new(Some(storage_path.clone()), true);

    assert_eq!(cache.memory_len(), 0);
    drop(cache);

    assert_eq!(read_schema_version(&storage_path), CURRENT_SCHEMA_VERSION);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_clear_removes_disk_and_memory_entries() {
    let storage_path = temp_storage();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
        assert_eq!(cache.memory_len(), 1);
        cache.clear();
        assert_eq!(cache.memory_len(), 0);
    }

    let cache = ImportCache::new(Some(storage_path.clone()), true);

    assert_eq!(cache.memory_len(), 0);
    assert!(cache.get("react@18.3.1::default").is_none());
    drop(cache);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn disk_cache_tracks_recent_entries_for_startup_prewarm() {
    let storage_path = temp_storage();

    let cache = ImportCache::new(Some(storage_path.clone()), true);
    cache.insert("left@1.0.0::default".to_owned(), result("left"));
    cache.insert("right@1.0.0::*".to_owned(), result("right"));

    assert_eq!(cache.recent_keys(1), vec!["right@1.0.0::*".to_owned()]);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn flush_to_disk_succeeds_with_nothing_dirty() {
    // Disk-enabled cache: insert already persisted synchronously, so a flush
    // has nothing to replay and must still succeed (and keep the entry).
    let storage_path = temp_storage();
    let cache = ImportCache::new(Some(storage_path.clone()), true);
    cache.insert("react@18.3.1::default".to_owned(), result("react"));

    cache.flush_to_disk().expect("flush should succeed");
    assert!(cache.get("react@18.3.1::default").is_some());

    // Disk-disabled cache: inserts never fail, so nothing is ever dirty.
    let memory_only = ImportCache::new(None, false);
    memory_only.insert("vue@3.4.0::default".to_owned(), result("vue"));
    memory_only.flush_to_disk().expect("flush should succeed");

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn flush_to_disk_persists_memory_entries_for_reload() {
    let storage_path = temp_storage();
    let key = "react@18.3.1::default".to_owned();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        cache.insert(key.clone(), result("react"));
        cache.flush_to_disk().expect("flush should succeed");
    }

    let reloaded = ImportCache::new(Some(storage_path.clone()), true);

    assert!(reloaded.get(&key).is_some());

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn insert_is_readable_before_flush_and_persists_after_flush() {
    let storage_path = temp_storage();
    let key = "react@18.3.1::default".to_owned();

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        cache.insert(key.clone(), result("react"));
        // Read-your-writes: visible immediately while still queued (unflushed).
        assert!(cache.get(&key).is_some(), "read-your-writes before flush");
        // No explicit flush — rely on Drop to drain the queue on teardown.
    }

    let reloaded = ImportCache::new(Some(storage_path.clone()), true);
    assert!(
        reloaded.get(&key).is_some(),
        "Drop should flush queued inserts so they survive reload"
    );

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

#[test]
fn many_inserts_flush_in_batches_without_loss() {
    let storage_path = temp_storage();
    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        for index in 0..200 {
            cache.insert(format!("pkg{index}@1.0.0::default"), result("pkg"));
        }
        cache.flush_to_disk().expect("flush should succeed");
    }

    let reloaded = ImportCache::new(Some(storage_path.clone()), true);
    assert_eq!(reloaded.recent_keys(1000).len(), 200);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}

fn cached(specifier: &str) -> CachedImport {
    CachedImport {
        result: result(specifier),
        dependency_fingerprints: Vec::new(),
        verified_generation: 0,
        verified_at: None,
        first_party: false,
        last_seq: Arc::new(AtomicU64::new(1)),
        persisted_seq: Arc::new(AtomicU64::new(1)),
    }
}

/// **The L2 WRITE gate, which nothing detected.** (ADR-0006, invariant 3: "the gate lives inside
/// each durable store".)
///
/// `DiskCache` gates a non-durable result on the way **in** *and* on the way **out**, and only the
/// read gate had a test — so the write gate could be deleted with the whole suite green. That is a
/// guard nobody is guarding: the two are not redundant, they cover different windows. The read gate
/// evicts a bad row that is *already on disk*; the write gate is what stops a transient outcome
/// reaching disk **at all**, which matters because redb outlives the process and because every future
/// reader of that row (a scan, a rollup, a byte-budget eviction, a `load_recent` prewarm) is one more
/// path that has to remember to ask.
///
/// So this asserts on the **raw redb table**, past both gates: a `panic` result — transient, a
/// property of this moment's scheduling and not of the package's bytes — must never become a row.
/// The measured result beside it must, or the assertion would pass on an empty database.
#[test]
fn a_transient_result_never_reaches_the_disk_table() {
    use import_lens_daemon::engine::stage;
    use redb::ReadableTableMetadata;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    {
        let disk = DiskCache::new(Some(storage.clone()), true);

        let mut transient = cached("flaky-lib@1::default");
        transient.result = ImportResult::unmeasured(
            "flaky-lib",
            stage::PANIC,
            "the build unwound into the boundary's catch_unwind",
            Vec::new(),
        );
        disk.insert("flaky-lib@1::default", &transient)
            .expect("a refused insert is a no-op, never an error");

        // The control: a real measurement, which MUST land, or an empty table would prove nothing.
        disk.insert("solid-lib@1::default", &cached("solid-lib@1::default"))
            .expect("a measured result is durable");

        disk.flush_pending_inserts();
    }

    let db = Database::open(db_path(&storage)).expect("reopen");
    let read = db.begin_read().expect("read");
    let table = read.open_table(CACHE_TABLE).expect("cache table");

    assert!(
        table.get("solid-lib@1::default").expect("read").is_some(),
        "test setup: a measured result must reach the disk table, or this test proves nothing"
    );
    assert!(
        table.get("flaky-lib@1::default").expect("read").is_none(),
        "a transient result must never become a row. The read gate would evict it on the way out, \
         but L2 outlives the process and every other reader of the table - the byte-budget scan, \
         the rollup, the prewarm's load_recent - is one more path that has to remember to ask"
    );
    assert_eq!(table.len().expect("len"), 1);

    drop(db);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn shard_rollup_sums_bytes_and_tracks_oldest_seq() {
    use import_lens_daemon::cache::disk::ShardRollup;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Three entries with strictly increasing seqs; persist them, then drop so the
    // batched inserts flush to the CACHE_TABLE.
    {
        let disk = DiskCache::new(Some(storage.clone()), true);
        for (index, key) in ["a@1::default", "b@1::default", "c@1::default"]
            .iter()
            .enumerate()
        {
            let mut entry = cached(key);
            // seq 10, 20, 30 → oldest is 10.
            entry.last_seq = Arc::new(AtomicU64::new((index as u64 + 1) * 10));
            disk.insert(key, &entry).expect("insert should queue");
        }
        disk.flush_pending_inserts();
    }

    // Authoritative expected total: the sum of the stored CACHE_TABLE value
    // lengths (the rollup tracks logical envelope bytes from these same rows).
    let expected_total = {
        let db = Database::open(db_path(&storage)).expect("reopen");
        let read = db.begin_read().expect("read");
        let table = read.open_table(CACHE_TABLE).expect("cache table");
        let mut total = 0_u64;
        for key in ["a@1::default", "b@1::default", "c@1::default"] {
            total += table.get(key).expect("get").expect("present").value().len() as u64;
        }
        total
    };

    let disk = DiskCache::new(Some(storage.clone()), true);
    let rollup: ShardRollup = disk.shard_rollup();
    assert_eq!(rollup.entry_count, 3, "all three entries counted");
    assert_eq!(
        rollup.total_bytes, expected_total,
        "bytes are summed exactly"
    );
    assert_eq!(rollup.oldest_seq, 10, "oldest_seq is the minimum last_seq");

    // An empty shard reports a never-selected sentinel.
    disk.clear();
    let empty = disk.shard_rollup();
    assert_eq!(empty.entry_count, 0);
    assert_eq!(empty.total_bytes, 0);
    assert_eq!(empty.oldest_seq, u64::MAX);

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn compaction_shrinks_the_file_after_heavy_eviction() {
    use import_lens_daemon::cache::disk::COMPACT_THRESHOLD;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Insert a batch, flush to disk, then evict almost all of it so most of the
    // file becomes reclaimable free space.
    let disk = DiskCache::new(Some(storage.clone()), true);
    let mut all_keys = Vec::new();
    for index in 0..1000 {
        let key = format!("pkg{index}@1.0.0::default");
        let mut entry = cached(&key);
        entry.last_seq = Arc::new(AtomicU64::new(index as u64 + 1));
        disk.insert(&key, &entry).expect("insert should queue");
        all_keys.push(key);
    }
    disk.flush_pending_inserts();

    let size_before = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();

    // Evict 950 of 1000 entries.
    let freed = disk.remove_keys(&all_keys[..950]);
    assert!(freed > 0, "eviction should free bytes");

    // redb reuses freed pages rather than shrinking, so the file is still large.
    let size_after_evict = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();

    // Compaction reclaims the free pages and shrinks the file — but only once the
    // shard is idle (the fill/evict above just touched it), so mark it idle first.
    disk.mark_idle_for_test();
    let compacted = disk.compact_if_fragmented(COMPACT_THRESHOLD);
    assert!(
        compacted,
        "a mostly-empty file must exceed the fragmentation threshold and compact"
    );

    let size_after_compact = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();
    assert!(
        size_after_compact < size_after_evict,
        "compaction must shrink the file: {size_after_compact} >= {size_after_evict}"
    );
    // Sanity: it is smaller than the fully-populated file too.
    assert!(size_after_compact < size_before);

    // Compaction must shrink the file WITHOUT losing surviving data: every
    // non-evicted entry must still decode and serve.
    for key in &all_keys[950..] {
        assert!(
            disk.get(key).is_some(),
            "entry {key} must survive compaction intact"
        );
    }

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn compaction_is_gated_on_shard_idleness() {
    use import_lens_daemon::cache::disk::COMPACT_THRESHOLD;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Fragment the shard: fill, flush, then evict almost all of it so most of
    // the file is reclaimable free space (crosses COMPACT_THRESHOLD).
    let disk = DiskCache::new(Some(storage.clone()), true);
    let mut all_keys = Vec::new();
    for index in 0..1000 {
        let key = format!("pkg{index}@1.0.0::default");
        let mut entry = cached(&key);
        entry.last_seq = Arc::new(AtomicU64::new(index as u64 + 1));
        disk.insert(&key, &entry).expect("insert should queue");
        all_keys.push(key);
    }
    disk.flush_pending_inserts();
    let freed = disk.remove_keys(&all_keys[..950]);
    assert!(freed > 0, "eviction should free bytes");

    // A surviving get marks the shard as just-accessed — the user is actively
    // analyzing it. A fragmented BUT actively-used shard must NOT be compacted:
    // Database::compact holds the exclusive lock across the whole rewrite, which
    // would block the user's concurrent gets.
    assert!(disk.get(&all_keys[950]).is_some());
    assert!(
        !disk.compact_if_fragmented(COMPACT_THRESHOLD),
        "a fragmented but recently-accessed shard must not be compacted"
    );
    let size_while_busy = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();

    // Once the shard goes idle (no get/insert within COMPACT_IDLE), the same
    // fragmented shard IS compacted and the file shrinks.
    disk.mark_idle_for_test();
    assert!(
        disk.compact_if_fragmented(COMPACT_THRESHOLD),
        "an idle fragmented shard must be compacted"
    );
    let size_after_compact = fs::metadata(db_path(&storage))
        .expect("db file should exist")
        .len();
    assert!(
        size_after_compact < size_while_busy,
        "compaction must shrink the idle shard: {size_after_compact} >= {size_while_busy}"
    );

    // Compaction must not drop surviving data.
    for key in &all_keys[950..] {
        assert!(
            disk.get(key).is_some(),
            "entry {key} must survive compaction intact"
        );
    }

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn stale_disk_get_does_not_deadlock_with_concurrent_compaction() {
    use import_lens_daemon::cache::key::fingerprints_for_paths;
    use std::sync::mpsc;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let dep = storage.join("dep.js");
    let key = "react@18.3.1::default";

    // A getter thread repeatedly lands on the Stale eviction path inside
    // `get_entry` (db read guard → decode → remove) while a compactor thread
    // hammers the exclusive write lock. Before the guard-scoping fix, the
    // re-entrant `remove` under a held read guard deadlocked against the queued
    // compaction writer; the channel timeout below is the failure signal.
    let disk = Arc::new(DiskCache::new(Some(storage.clone()), true));

    let (done_tx, done_rx) = mpsc::channel();
    let getter = {
        let disk = Arc::clone(&disk);
        let dep = dep.clone();
        std::thread::spawn(move || {
            for round in 0..50 {
                // Fresh content each round, fingerprinted, then changed → Stale.
                fs::write(&dep, format!("export const v = {round};")).expect("dep write");
                let mut entry = cached("react");
                entry.dependency_fingerprints = fingerprints_for_paths([dep.clone()]);
                disk.insert(key, &entry).expect("insert should queue");
                disk.flush_pending_inserts();
                fs::write(
                    &dep,
                    format!("export const v = 'changed {round} with longer bytes';"),
                )
                .expect("dep rewrite");
                // Stale → the re-entrant remove path.
                assert!(disk.get_with_freshness(key).is_none());
            }
            let _ = done_tx.send(());
        })
    };
    let compactor = {
        let disk = Arc::clone(&disk);
        std::thread::spawn(move || {
            for _ in 0..200 {
                // Force the idle gate open each iteration so this still drives the
                // exclusive compaction writer against the concurrent stale-get
                // remove path (threshold 0.0 → attempts the exclusive write lock).
                disk.mark_idle_for_test();
                let _ = disk.compact_if_fragmented(0.0);
            }
        })
    };

    assert!(
        done_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
        "stale disk get deadlocked against a concurrent compaction"
    );
    getter.join().expect("getter thread");
    compactor.join().expect("compactor thread");

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn a_second_open_of_a_live_shard_degrades_without_deleting_data() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let key = "react@18.3.1::default";

    let first = DiskCache::new(Some(storage.clone()), true);
    first.insert(key, &cached("react")).expect("insert");
    first.flush_pending_inserts();

    // A second open of the same file in-process hits DatabaseAlreadyOpen. The
    // guard must degrade to a disabled cache — NEVER fall through to the
    // recreate path, which unlinks the live shard's file and silently destroys
    // the first handle's data.
    let second = DiskCache::new(Some(storage.clone()), true);
    assert!(
        !second.is_available(),
        "the racing open must degrade to a disabled cache"
    );
    assert!(second.get(key).is_none(), "a disabled cache serves nothing");
    second
        .insert(key, &cached("react"))
        .expect("a disabled cache accepts inserts as no-ops");
    drop(second);

    assert!(
        first.get(key).is_some(),
        "the live handle must be unaffected by the degraded open"
    );
    drop(first);

    let reopened = DiskCache::new(Some(storage.clone()), true);
    assert!(
        reopened.get(key).is_some(),
        "the data must survive the degraded open"
    );
    drop(reopened);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn loading_persisted_seqs_advances_the_recency_clock() {
    use import_lens_daemon::cache::recency::RecencyClock;

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let key = "react@18.3.1::default";

    // Persist an entry stamped far ahead of the live clock, simulating a prior
    // session that accumulated many accesses.
    let future_seq = RecencyClock::next_seq() + 1_000_000;
    {
        let disk = DiskCache::new(Some(storage.clone()), true);
        let mut entry = cached("react");
        entry.last_seq = Arc::new(AtomicU64::new(future_seq));
        disk.insert(key, &entry).expect("insert should queue");
        disk.flush_pending_inserts();
    }

    // Reload and scan, as startup would. The load path must observe the
    // persisted seq: without it, a fresh process (clock reset to 1) would stamp
    // new interactive accesses BELOW the durable entries of the last session,
    // inverting LRU order for the evictor.
    let disk = DiskCache::new(Some(storage.clone()), true);
    let rollup = disk.shard_rollup();
    assert_eq!(rollup.oldest_seq, future_seq);
    assert!(
        RecencyClock::next_seq() > future_seq,
        "the live clock must advance past every persisted seq on load"
    );
    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn memory_promoted_entries_are_shielded_from_disk_eviction() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    let cache = ImportCache::new(Some(storage.clone()), true);
    cache.insert("a@1.0.0::default".to_owned(), result("a"));
    cache.insert("b@1.0.0::default".to_owned(), result("b"));
    cache.flush_to_disk().expect("flush");

    // Promote `a` in memory only (interactive hit bumps the in-memory seq; the
    // persisted seq is untouched until the next flush). By persisted seq alone,
    // `a` (inserted first) is the older entry — the eviction filter must
    // recognize the promotion and refuse to offer `a` as a victim.
    assert!(cache.get("a@1.0.0::default").is_some());

    let victims = cache.lowest_seq_disk_keys(10, 0);
    assert!(
        victims.contains(&"b@1.0.0::default".to_owned()),
        "the un-promoted entry is the correct victim: {victims:?}"
    );
    assert!(
        !victims.contains(&"a@1.0.0::default".to_owned()),
        "a memory-promoted entry must never be offered for eviction: {victims:?}"
    );

    drop(cache);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn eviction_pages_past_an_all_hot_lowest_batch_to_cold_entries_deeper() {
    use import_lens_daemon::cache::budget::{EVICTION_BATCH, EVICTION_FLOOR};

    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let cache = ImportCache::new(Some(storage.clone()), true);

    // Layout by persisted seq (insert order stamps a strictly increasing seq):
    //   [ 0 .. BATCH )         -> will be made memory-hot (lowest persisted seqs)
    //   [ BATCH .. 3*BATCH )   -> genuinely cold, evictable, deeper in the shard
    //   [ 3*BATCH .. end )     -> newest FLOOR, floor-protected
    let entry_count = EVICTION_FLOOR as usize + EVICTION_BATCH * 3;
    let key_of = |index: usize| format!("pkg{index:05}@1.0.0::default");
    for index in 0..entry_count {
        cache.insert(key_of(index), result("pkg"));
    }
    cache
        .flush_to_disk()
        .expect("flush should persist the seqs");

    // Promote the lowest EVICTION_BATCH persisted-seq entries in memory only: an
    // interactive get bumps last_seq past the persisted seq the disk index sorted
    // them by, with no flush, so all BATCH of them are memory-hot. These are
    // exactly the keys the disk index offers first.
    let hot: Vec<String> = (0..EVICTION_BATCH).map(key_of).collect();
    for key in &hot {
        assert!(
            cache.get(key).is_some(),
            "hot fixture entry should be resident"
        );
    }

    // The evictor asks for a full EVICTION_BATCH of victims beyond the floor. The
    // lowest BATCH are all hot, so a "take BATCH, drop hot, give up if empty"
    // selection would retire the shard (Finding 10c). Paging past the hot batch
    // must instead find a full batch of genuinely-cold victims.
    let victims = cache.lowest_seq_disk_keys(EVICTION_BATCH, EVICTION_FLOOR);

    assert_eq!(
        victims.len(),
        EVICTION_BATCH,
        "must page past the all-hot lowest batch to a full cold batch, not retire empty"
    );
    for key in &hot {
        assert!(
            !victims.contains(key),
            "a memory-hot entry must never be offered as a victim: {key}"
        );
    }
    // The victims are exactly the lowest-seq COLD keys, immediately past the hot
    // batch and before the floor: entries [BATCH, 2*BATCH).
    let mut sorted = victims.clone();
    sorted.sort();
    let expected: Vec<String> = (EVICTION_BATCH..EVICTION_BATCH * 2).map(key_of).collect();
    assert_eq!(
        sorted, expected,
        "victims are the lowest-seq cold keys just past the hot batch"
    );

    drop(cache);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn flush_persists_promoted_recency_for_the_next_session() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    {
        let cache = ImportCache::new(Some(storage.clone()), true);
        cache.insert("a@1.0.0::default".to_owned(), result("a"));
        cache.insert("b@1.0.0::default".to_owned(), result("b"));
        cache.flush_to_disk().expect("first flush");

        // `b` was inserted later, so by insert seq it is the most recent. Promote
        // `a` interactively, then flush: the recency sweep must re-persist `a`
        // with its promoted seq so the NEXT session sees `a` as most recent.
        assert!(cache.get("a@1.0.0::default").is_some());
        cache
            .flush_to_disk()
            .expect("second flush persists the promotion");
    }

    let reloaded = ImportCache::new_with_recent_preload_limit(Some(storage.clone()), true, 0);
    assert_eq!(
        reloaded.recent_keys(1),
        vec!["a@1.0.0::default".to_owned()],
        "session recency must survive the restart"
    );
    drop(reloaded);
    fs::remove_dir_all(storage).expect("cleanup");
}

// --- C1: incremental summary + (last_seq, key) secondary index ---------------
//
// These exercise the O(1) summary / O(log N) index accounting through the public
// DiskCache API. The drift property test is the anti-drift guard: after a random
// op sequence, the incrementally-maintained rollup must be bit-identical to a
// fresh `rebuild_summary_from_scan` recomputation.

#[test]
fn summary_tracks_bytes_count_seq_across_insert_replace_remove() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let disk = DiskCache::new(Some(storage.clone()), true);

    // Three entries with distinct seqs (10, 20, 30) and distinct sizes.
    for (index, key) in ["a@1::default", "bb@1::default", "ccc@1::default"]
        .iter()
        .enumerate()
    {
        let mut entry = cached(key);
        entry.last_seq = Arc::new(AtomicU64::new((index as u64 + 1) * 10));
        disk.insert(key, &entry).expect("insert should queue");
    }
    disk.flush_pending_inserts();

    // Replace `bb` with a strictly larger value at a new seq (25): exercises the
    // replace path (old index entry removed, byte delta applied, count unchanged).
    {
        let mut bigger = cached("bb@1::default");
        bigger.result.confidence_reasons = vec!["padding".repeat(200)];
        bigger.last_seq = Arc::new(AtomicU64::new(25));
        disk.insert("bb@1::default", &bigger)
            .expect("replace should queue");
    }
    disk.flush_pending_inserts();

    // Remove the oldest (a@10).
    disk.remove_keys(&["a@1::default".to_owned()]);

    // The incrementally-maintained rollup must equal a fresh full-scan rebuild.
    let incremental = disk.shard_rollup();
    disk.rebuild_summary_from_scan();
    let rebuilt = disk.shard_rollup();
    assert_eq!(
        incremental, rebuilt,
        "incrementally-maintained summary must match a full scan"
    );

    // Direct invariants: bb(25) and ccc(30) remain; a(10) is gone.
    assert_eq!(incremental.entry_count, 2, "3 inserted, 1 removed");
    assert_eq!(
        incremental.oldest_seq, 25,
        "a(10) gone; bb replaced to 25; ccc=30 -> oldest 25"
    );
    assert!(incremental.total_bytes > 0);

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn lowest_seq_keys_is_indexed_and_excludes_floor() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let disk = DiskCache::new(Some(storage.clone()), true);

    // seqs 10, 20, 30, 40, 50 for k1..k5.
    for (index, key) in ["k1", "k2", "k3", "k4", "k5"].iter().enumerate() {
        let mut entry = cached(key);
        entry.last_seq = Arc::new(AtomicU64::new((index as u64 + 1) * 10));
        disk.insert(key, &entry).expect("insert should queue");
    }
    disk.flush_pending_inserts();

    // floor=2 protects the two newest (k5@50, k4@40); n=2 -> the two lowest
    // eligible, ascending by seq.
    assert_eq!(
        disk.lowest_seq_keys(2, 2),
        vec![("k1".to_owned(), 10), ("k2".to_owned(), 20)],
        "returns the n lowest-seq keys beyond the newest floor, ascending"
    );

    // n larger than the eligible set -> all eligible (3), still floor-excluded.
    assert_eq!(
        disk.lowest_seq_keys(10, 2),
        vec![
            ("k1".to_owned(), 10),
            ("k2".to_owned(), 20),
            ("k3".to_owned(), 30),
        ],
    );

    // floor at or above the count -> nothing eligible.
    assert!(disk.lowest_seq_keys(10, 5).is_empty());
    assert!(disk.lowest_seq_keys(10, 99).is_empty());

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn oldest_seq_advances_after_evicting_the_oldest() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let disk = DiskCache::new(Some(storage.clone()), true);

    for (index, key) in ["a", "b", "c"].iter().enumerate() {
        let mut entry = cached(key);
        entry.last_seq = Arc::new(AtomicU64::new((index as u64 + 1) * 10));
        disk.insert(key, &entry).expect("insert should queue");
    }
    disk.flush_pending_inserts();
    assert_eq!(disk.shard_rollup().oldest_seq, 10);

    assert!(
        disk.remove_keys(&["a".to_owned()]) > 0,
        "eviction frees bytes"
    );
    assert_eq!(
        disk.shard_rollup().oldest_seq,
        20,
        "oldest_seq advances to the next-lowest after evicting the oldest"
    );

    disk.remove_keys(&["b".to_owned()]);
    assert_eq!(disk.shard_rollup().oldest_seq, 30);

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn drift_property_random_ops_keep_summary_equal_to_full_scan() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");
    let disk = DiskCache::new(Some(storage.clone()), true);

    // Deterministic LCG (reproducible; no external RNG that breaks reproducibility).
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut rng = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 33
    };

    let key_space = 20_u64;
    for _ in 0..800 {
        let roll = rng();
        let key = format!("pkg{}@1.0.0::default", roll % key_space);
        match roll % 3 {
            0 | 1 => {
                let mut entry = cached(&key);
                entry.last_seq = Arc::new(AtomicU64::new((rng() % 5000) + 1));
                if rng() % 2 == 0 {
                    entry.result.confidence_reasons = vec!["z".repeat((rng() % 400) as usize)];
                }
                disk.insert(&key, &entry).expect("insert should queue");
                // Interleave flushes so replaces hit both the pending and the
                // committed paths.
                if rng() % 4 == 0 {
                    disk.flush_pending_inserts();
                }
            }
            _ => {
                disk.remove_keys(&[key]);
            }
        }
    }

    // The anti-drift guard: the incrementally-maintained summary + index must be
    // identical to a fresh recomputation from a full CACHE_TABLE scan.
    let incremental = disk.shard_rollup();
    disk.rebuild_summary_from_scan();
    let rebuilt = disk.shard_rollup();
    assert_eq!(
        incremental, rebuilt,
        "incremental accounting drifted from a full scan: {incremental:?} != {rebuilt:?}"
    );

    // The index-backed victim list stays globally ascending by seq.
    let victims = disk.lowest_seq_keys(64, 0);
    for pair in victims.windows(2) {
        assert!(
            pair[0].1 <= pair[1].1,
            "lowest_seq_keys must be ascending by seq"
        );
    }

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}

#[test]
fn opening_a_v7_shard_without_summary_rows_heals_by_rebuilding() {
    let storage = temp_storage();
    fs::create_dir_all(&storage).expect("storage dir");

    // Simulate a drifted v7 shard: CACHE_TABLE has entries but SUMMARY has no
    // rows. `decode_last_seq` reads the 8-byte LE prefix, so `[seq][payload]`
    // values give the rebuild real seqs and lengths without a full envelope.
    let mut expected_total = 0_u64;
    {
        let db = Database::create(db_path(&storage)).expect("create db");
        let write = db.begin_write().expect("begin");
        {
            let mut meta = write.open_table(METADATA_TABLE).expect("meta table");
            meta.insert(SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION)
                .expect("write schema version");
            let mut cache = write.open_table(CACHE_TABLE).expect("cache table");
            for (key, seq) in [
                ("k1", 50_u64),
                ("k2", 40),
                ("k3", 30),
                ("k4", 20),
                ("k5", 10),
            ] {
                let mut value = seq.to_le_bytes().to_vec();
                value.extend_from_slice(b"payload");
                expected_total += value.len() as u64;
                cache.insert(key, value.as_slice()).expect("write row");
            }
            // Deliberately leave SUMMARY / SEQ_INDEX untouched (absent rows).
        }
        write.commit().expect("commit");
    }

    // Opening through DiskCache must detect the entry_count vs. row-count mismatch
    // and rebuild the summary + index once from a scan.
    let disk = DiskCache::new(Some(storage.clone()), true);
    let rollup = disk.shard_rollup();
    assert_eq!(
        rollup.entry_count, 5,
        "heal must rebuild the entry count from a full scan"
    );
    assert_eq!(
        rollup.total_bytes, expected_total,
        "heal must rebuild the total bytes"
    );
    assert_eq!(
        rollup.oldest_seq, 10,
        "heal must rebuild the seq index (oldest first)"
    );
    // The rebuilt index must also drive lowest_seq_keys correctly.
    assert_eq!(
        disk.lowest_seq_keys(2, 0),
        vec![("k5".to_owned(), 10), ("k4".to_owned(), 20)],
    );

    drop(disk);
    fs::remove_dir_all(storage).expect("cleanup");
}
