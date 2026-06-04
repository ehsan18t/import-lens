use import_lens_daemon::{
    cache::memory::ImportCache,
    ipc::protocol::{ImportDiagnostic, ImportResult},
};
use redb::{Database, ReadableDatabase, TableDefinition};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const RECENTS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("cache_recents");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 3;

mod common;

fn temp_storage() -> PathBuf {
    common::temp_workspace("import-lens-cache")
}

fn db_path(storage_path: &Path) -> PathBuf {
    storage_path.join("importlens.redb")
}

fn result(specifier: &str) -> ImportResult {
    ImportResult {
        specifier: specifier.to_owned(),
        raw_bytes: 10,
        minified_bytes: 8,
        gzip_bytes: 7,
        brotli_bytes: 6,
        zstd_bytes: 5,
        cache_hit: false,
        side_effects: false,
        truly_treeshakeable: true,
        is_cjs: false,
        error: None,
        diagnostics: vec![ImportDiagnostic {
            stage: "test".to_owned(),
            message: "cached".to_owned(),
            details: Vec::new(),
        }],
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
    }
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

fn read_recent_timestamp(storage_path: &Path, key: &str) -> u64 {
    let db = Database::open(db_path(storage_path)).expect("cache database should open");
    let read_txn = db.begin_read().expect("read transaction should begin");
    let table = read_txn
        .open_table(RECENTS_TABLE)
        .expect("recents table should exist");

    table
        .get(key)
        .expect("recent key should be readable")
        .expect("recent key should exist")
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

    {
        let mut recents = write_txn
            .open_table(RECENTS_TABLE)
            .expect("recents table should open");
        recents
            .insert("react@18.3.1::default", 1)
            .expect("recent entry should be written");
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

    {
        let mut recents = write_txn
            .open_table(RECENTS_TABLE)
            .expect("recents table should open");
        recents
            .insert(key, 1)
            .expect("recent key should be written");
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
        for index in 0..5 {
            cache.insert(format!("pkg-{index}@1.0.0::default"), result("pkg"));
            thread::sleep(Duration::from_millis(2));
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
fn disk_cache_coalesces_repeated_memory_hit_recency_writes() {
    let storage_path = temp_storage();
    let key = "react@18.3.1::default";

    {
        let cache = ImportCache::new(Some(storage_path.clone()), true);
        cache.insert(key.to_owned(), result("react"));
    }
    let first_timestamp = read_recent_timestamp(&storage_path, key);
    thread::sleep(Duration::from_millis(2));

    {
        let cache = ImportCache::new_with_recent_preload_limit(Some(storage_path.clone()), true, 1);
        for _ in 0..3 {
            assert!(
                cache
                    .get(key)
                    .expect("memory hit should return result")
                    .cache_hit
            );
        }

        assert_eq!(cache.pending_recency_touch_count(), 1);
    }

    assert!(read_recent_timestamp(&storage_path, key) > first_timestamp);

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
    thread::sleep(Duration::from_millis(2));
    cache.insert("right@1.0.0::*".to_owned(), result("right"));

    assert_eq!(cache.recent_keys(1), vec!["right@1.0.0::*".to_owned()]);

    fs::remove_dir_all(storage_path).expect("temp storage should be removed");
}
