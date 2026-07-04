use crate::{
    cache::key::{
        ANALYZER_VERSION, CacheIdentityV3, FileFingerprint, cache_key_is_orphan,
        cache_key_matches_any_package, decode_cache_identity, fingerprints_are_current,
    },
    cache::memory::CachedImport,
    ipc::protocol::{ImportResult, ModuleContribution},
    time::unix_millis_now,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicU64},
};

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const RECENTS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("cache_recents");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 4;
const RECENCY_TOUCH_FLUSH_BATCH: usize = 64;
const INSERT_FLUSH_BATCH: usize = 64;

#[derive(Debug, Clone)]
struct PendingInsert {
    bytes: Vec<u8>,
    recorded_at_millis: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    analyzer_version: String,
    result: ImportResult,
    package_identity: Option<CacheIdentityV3>,
    dependency_fingerprints: Vec<FileFingerprint>,
    full_contributions: Vec<ModuleContribution>,
}

#[derive(Debug, Default)]
pub struct DiskCache {
    db: Option<Database>,
    pending_touches: Mutex<HashMap<String, u64>>,
    // Serialized envelopes awaiting a batched commit; drained at a size
    // threshold, on recent_keys, on recycle (flush_to_disk), and on Drop.
    pending_inserts: Mutex<HashMap<String, PendingInsert>>,
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
            db: Self::open_database(&storage_path),
            pending_touches: Mutex::new(HashMap::new()),
            pending_inserts: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &str) -> Option<CachedImport> {
        self.get_entry(key, true)
    }

    pub fn load_recent(&self, limit: usize) -> Vec<(String, CachedImport)> {
        if limit == 0 {
            return Vec::new();
        }

        self.recent_keys(limit)
            .into_iter()
            .filter_map(|key| self.get_entry(&key, false).map(|cached| (key, cached)))
            .collect()
    }

    fn get_entry(&self, key: &str, touch: bool) -> Option<CachedImport> {
        // Read-your-writes: a queued insert not yet flushed is not in the table.
        if let Some(cached) = self.pending_insert_entry(key) {
            if touch {
                self.touch(key);
            }
            return Some(cached);
        }

        let db = self.db.as_ref()?;
        let read_txn = db.begin_read().ok()?;
        let table = read_txn.open_table(CACHE_TABLE).ok()?;
        let value = table.get(key).ok()??;

        let bytes = value.value();
        let cached = match decode_cached_result(bytes) {
            Some(cached) => cached,
            None => {
                drop(table);
                drop(read_txn);
                self.remove(key);
                return None;
            }
        };
        if !fingerprints_are_current(&cached.dependency_fingerprints) {
            drop(table);
            drop(read_txn);
            self.remove(key);
            return None;
        }
        if touch {
            self.touch(key);
        }
        Some(cached)
    }

    pub fn insert(&self, key: &str, cached: &CachedImport) -> Result<(), String> {
        if self.db.is_none() {
            return Ok(());
        }

        let mut persisted = cached.clone();
        persisted.result.cache_hit = false;

        let envelope = cache_envelope(key, persisted);
        let bytes = rmp_serde::to_vec(&envelope)
            .map_err(|error| format!("failed to serialize cache entry: {error}"))?;

        // Queue for a batched commit instead of one durable transaction per
        // entry; a cold parallel batch otherwise serialized N fsyncs on redb's
        // single writer.
        let should_flush = match self.pending_inserts.lock() {
            Ok(mut pending) => {
                pending.insert(
                    key.to_owned(),
                    PendingInsert {
                        bytes,
                        recorded_at_millis: unix_millis_now(),
                    },
                );
                pending.len() >= INSERT_FLUSH_BATCH
            }
            Err(_) => return Err("cache pending-insert lock poisoned".to_owned()),
        };
        self.remove_pending_touch(key);
        if should_flush {
            self.flush_pending_inserts();
        }
        Ok(())
    }

    pub fn flush_pending_inserts(&self) {
        let db = match self.db.as_ref() {
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

        if let Err(error) = write_pending_inserts(db, &pending) {
            if let Ok(mut current) = self.pending_inserts.lock() {
                for (key, entry) in pending {
                    current.entry(key).or_insert(entry);
                }
            }
            cache_warn(format!("failed to flush cache inserts: {error}"));
        }
    }

    fn pending_insert_entry(&self, key: &str) -> Option<CachedImport> {
        let bytes = {
            let pending = self.pending_inserts.lock().ok()?;
            pending.get(key)?.bytes.clone()
        };
        let cached = decode_cached_result(&bytes)?;
        if !fingerprints_are_current(&cached.dependency_fingerprints) {
            self.remove(key);
            return None;
        }
        Some(cached)
    }

    pub fn touch(&self, key: &str) {
        if self.db.is_none() {
            return;
        }

        let should_flush = match self.pending_touches.lock() {
            Ok(mut pending_touches) => {
                pending_touches.insert(key.to_owned(), unix_millis_now());
                pending_touches.len() >= RECENCY_TOUCH_FLUSH_BATCH
            }
            Err(_) => return,
        };

        if should_flush {
            self.flush_pending_touches();
        }
    }

    pub fn flush_pending_touches(&self) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };
        let pending_touches = match self.pending_touches.lock() {
            Ok(mut pending_touches) => {
                if pending_touches.is_empty() {
                    return;
                }
                std::mem::take(&mut *pending_touches)
            }
            Err(_) => return,
        };

        if let Err(error) = write_pending_touches(db, &pending_touches) {
            if let Ok(mut current) = self.pending_touches.lock() {
                merge_pending_touches(&mut current, pending_touches);
            }
            cache_warn(format!("failed to flush cache recency touches: {error}"));
        }
    }

    pub fn pending_touch_len(&self) -> usize {
        self.pending_touches
            .lock()
            .map(|pending_touches| pending_touches.len())
            .unwrap_or(0)
    }

    pub fn remove(&self, key: &str) {
        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.remove(key);
        }
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                let _ = table.remove(key);
            }
            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                let _ = recents.remove(key);
            }
            let _ = write_txn.commit();
        }
    }

    pub fn recent_keys(&self, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }

        self.flush_pending_inserts();
        self.flush_pending_touches();

        let db = match self.db.as_ref() {
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
        let recents = match read_txn.open_table(RECENTS_TABLE) {
            Ok(table) => table,
            Err(error) => {
                cache_warn(format!("failed to open recent cache table: {error}"));
                return Vec::new();
            }
        };
        let iter = match recents.iter() {
            Ok(iter) => iter,
            Err(error) => {
                cache_warn(format!("failed to iterate recent cache table: {error}"));
                return Vec::new();
            }
        };
        let mut keys = iter
            .filter_map(|entry| {
                let (key, timestamp) = entry.ok()?;
                Some((key.value().to_owned(), timestamp.value()))
            })
            .collect::<Vec<_>>();

        if keys.len() > limit {
            keys.select_nth_unstable_by(limit, compare_recent_keys);
            keys.truncate(limit);
        }
        keys.sort_by(compare_recent_keys);
        keys.into_iter().map(|(key, _)| key).collect()
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
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            let mut keys_to_remove = Vec::new();

            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                if let Ok(iter) = table.iter() {
                    for result in iter {
                        if let Ok((key, _)) = result
                            && cache_key_matches_any_package(key.value(), package_names)
                        {
                            keys_to_remove.push(key.value().to_owned());
                        }
                    }
                }

                for key in &keys_to_remove {
                    let _ = table.remove(key.as_str());
                }
            }

            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                for key in &keys_to_remove {
                    let _ = recents.remove(key.as_str());
                }
            }

            let _ = write_txn.commit();

            for key in keys_to_remove {
                self.remove_pending_touch(&key);
            }
        }
    }

    /// Drops orphaned entries (release-stale analyzer version, or a resolved
    /// package/entry path that no longer exists). Scans once under a read txn,
    /// then removes under a short write txn. Returns the number removed.
    pub fn purge_orphan_entries(&self, current_analyzer_version: &str) -> usize {
        let db = match self.db.as_ref() {
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

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                for key in &orphan_keys {
                    let _ = table.remove(key.as_str());
                }
            }
            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                for key in &orphan_keys {
                    let _ = recents.remove(key.as_str());
                }
            }
            let _ = write_txn.commit();
        }

        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.retain(|key, _| !cache_key_is_orphan(key, current_analyzer_version));
        }

        orphan_keys.len()
    }

    pub fn clear(&self) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                let mut keys_to_remove = Vec::new();
                if let Ok(iter) = table.iter() {
                    for (key, _) in iter.flatten() {
                        keys_to_remove.push(key.value().to_owned());
                    }
                }
                for key in keys_to_remove {
                    let _ = table.remove(key.as_str());
                }
            }
            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                let mut keys_to_remove = Vec::new();
                if let Ok(iter) = recents.iter() {
                    for (key, _) in iter.flatten() {
                        keys_to_remove.push(key.value().to_owned());
                    }
                }
                for key in keys_to_remove {
                    let _ = recents.remove(key.as_str());
                }
            }
            let _ = write_txn.commit();
        }
        self.clear_pending_touches();
        if let Ok(mut pending) = self.pending_inserts.lock() {
            pending.clear();
        }
    }

    fn disabled() -> Self {
        Self {
            db: None,
            pending_touches: Mutex::new(HashMap::new()),
            pending_inserts: Mutex::new(HashMap::new()),
        }
    }

    fn remove_pending_touch(&self, key: &str) {
        if let Ok(mut pending_touches) = self.pending_touches.lock() {
            pending_touches.remove(key);
        }
    }

    fn clear_pending_touches(&self) {
        if let Ok(mut pending_touches) = self.pending_touches.lock() {
            pending_touches.clear();
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
            Err(error) => {
                cache_warn(format!(
                    "failed to open cache database {}: {error}",
                    db_path.display()
                ));
                return Self::recreate_database(&db_path);
            }
        };

        match Self::ensure_schema(&db, !db_existed) {
            Ok(()) => Some(db),
            Err(error) => {
                cache_warn(format!(
                    "cache database {} is unusable: {error}",
                    db_path.display()
                ));
                drop(db);
                Self::recreate_database(&db_path)
            }
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

    fn ensure_schema(db: &Database, initialize_missing_schema: bool) -> Result<(), String> {
        let write_txn = db
            .begin_write()
            .map_err(|error| format!("failed to begin schema transaction: {error}"))?;

        let version = {
            let mut metadata = write_txn
                .open_table(METADATA_TABLE)
                .map_err(|error| format!("failed to open metadata table: {error}"))?;
            let current = metadata
                .get(SCHEMA_VERSION_KEY)
                .map_err(|error| format!("failed to read schema version: {error}"))?
                .map(|value| value.value());

            match current {
                Some(value) => value,
                None if initialize_missing_schema => {
                    metadata
                        .insert(SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION)
                        .map_err(|error| format!("failed to write schema version: {error}"))?;
                    CURRENT_SCHEMA_VERSION
                }
                None => return Err("schema version is missing".to_owned()),
            }
        };

        if version != CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "schema version {version} does not match {CURRENT_SCHEMA_VERSION}"
            ));
        }

        {
            write_txn
                .open_table(CACHE_TABLE)
                .map_err(|error| format!("failed to open cache table: {error}"))?;
        }
        {
            write_txn
                .open_table(RECENTS_TABLE)
                .map_err(|error| format!("failed to open recent cache table: {error}"))?;
        }

        write_txn
            .commit()
            .map_err(|error| format!("failed to commit schema transaction: {error}"))
    }
}

impl Drop for DiskCache {
    fn drop(&mut self) {
        // Inserts first: each also writes its recents row.
        self.flush_pending_inserts();
        self.flush_pending_touches();
    }
}

fn write_pending_inserts(
    db: &Database,
    pending: &HashMap<String, PendingInsert>,
) -> Result<(), String> {
    let write_txn = db
        .begin_write()
        .map_err(|error| format!("failed to begin cache write: {error}"))?;

    {
        let mut table = write_txn
            .open_table(CACHE_TABLE)
            .map_err(|error| format!("failed to open cache table: {error}"))?;
        let mut recents = write_txn
            .open_table(RECENTS_TABLE)
            .map_err(|error| format!("failed to open recents table: {error}"))?;
        for (key, entry) in pending {
            table
                .insert(key.as_str(), entry.bytes.as_slice())
                .map_err(|error| format!("failed to insert cache entry: {error}"))?;
            // Never lower an existing (newer) recents timestamp: the insert and
            // touch queues flush independently, so an insert flushing after a
            // later touch of the same key must not regress its recency and demote
            // it out of the startup preload/prewarm set.
            let keep_existing = recents
                .get(key.as_str())
                .map_err(|error| format!("failed to read recents table: {error}"))?
                .map(|existing| existing.value() >= entry.recorded_at_millis)
                .unwrap_or(false);
            if !keep_existing {
                recents
                    .insert(key.as_str(), entry.recorded_at_millis)
                    .map_err(|error| format!("failed to update recents table: {error}"))?;
            }
        }
    }

    write_txn
        .commit()
        .map_err(|error| format!("failed to commit cache write: {error}"))
}

fn write_pending_touches(
    db: &Database,
    pending_touches: &HashMap<String, u64>,
) -> Result<(), String> {
    let write_txn = db
        .begin_write()
        .map_err(|error| format!("failed to begin recency touch write: {error}"))?;

    {
        let mut recents = write_txn
            .open_table(RECENTS_TABLE)
            .map_err(|error| format!("failed to open recents table: {error}"))?;
        for (key, timestamp) in pending_touches {
            recents
                .insert(key.as_str(), *timestamp)
                .map_err(|error| format!("failed to update recents table: {error}"))?;
        }
    }

    write_txn
        .commit()
        .map_err(|error| format!("failed to commit recency touch write: {error}"))
}

fn merge_pending_touches(current: &mut HashMap<String, u64>, restored: HashMap<String, u64>) {
    for (key, timestamp) in restored {
        current
            .entry(key)
            .and_modify(|current_timestamp| {
                *current_timestamp = (*current_timestamp).max(timestamp);
            })
            .or_insert(timestamp);
    }
}

fn compare_recent_keys(left: &(String, u64), right: &(String, u64)) -> Ordering {
    right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0))
}

fn cache_envelope(key: &str, cached: CachedImport) -> CacheEnvelope {
    let package_identity = decode_cache_identity(key);
    let mut dependency_fingerprints = cached.dependency_fingerprints.clone();
    if let Some(identity) = &package_identity {
        if let Some(fingerprint) = &identity.manifest_fingerprint {
            dependency_fingerprints.push(fingerprint.clone());
        }
        if let Some(fingerprint) = &identity.entry_fingerprint {
            dependency_fingerprints.push(fingerprint.clone());
        }
    }

    CacheEnvelope {
        analyzer_version: ANALYZER_VERSION.to_owned(),
        full_contributions: if cached.result.internal_contributions.is_empty() {
            cached.result.module_breakdown.clone().unwrap_or_default()
        } else {
            cached.result.internal_contributions.clone()
        },
        result: cached.result,
        package_identity,
        dependency_fingerprints,
    }
}

fn decode_cached_result(bytes: &[u8]) -> Option<CachedImport> {
    if let Ok(envelope) = rmp_serde::from_slice::<CacheEnvelope>(bytes) {
        if envelope.analyzer_version == ANALYZER_VERSION {
            let mut result = envelope.result;
            result.internal_contributions = envelope.full_contributions;
            return Some(CachedImport {
                result,
                dependency_fingerprints: envelope.dependency_fingerprints,
                verified_generation: 0,
                verified_at_millis: 0,
                last_used_millis: Arc::new(AtomicU64::new(unix_millis_now())),
            });
        }
        return None;
    }

    rmp_serde::from_slice::<ImportResult>(bytes)
        .ok()
        .map(|result| CachedImport {
            result,
            dependency_fingerprints: Vec::new(),
            verified_generation: 0,
            verified_at_millis: 0,
            last_used_millis: Arc::new(AtomicU64::new(unix_millis_now())),
        })
}

fn cache_warn(message: String) {
    crate::logging::log_warn("cache", message);
}

#[cfg(test)]
mod tests {
    use super::merge_pending_touches;
    use std::collections::HashMap;

    #[test]
    fn merge_pending_touches_preserves_newer_pending_timestamp() {
        let mut current = HashMap::from([("react".to_owned(), 30), ("vue".to_owned(), 20)]);
        let restored = HashMap::from([("react".to_owned(), 10), ("svelte".to_owned(), 40)]);

        merge_pending_touches(&mut current, restored);

        assert_eq!(current.get("react"), Some(&30));
        assert_eq!(current.get("vue"), Some(&20));
        assert_eq!(current.get("svelte"), Some(&40));
    }

    #[test]
    fn write_pending_inserts_does_not_lower_a_newer_recents_timestamp() {
        use super::{PendingInsert, RECENTS_TABLE, write_pending_inserts};
        use redb::{Database, ReadableDatabase};

        let dir = std::env::temp_dir().join(format!(
            "import-lens-disk-recents-monotonic-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        let db = Database::create(dir.join("recents.redb")).expect("db should open");

        // Seed a newer recents timestamp, as a touch flush would.
        {
            let txn = db.begin_write().expect("write txn");
            {
                let mut recents = txn.open_table(RECENTS_TABLE).expect("recents table");
                recents.insert("k", 2_000_u64).expect("seed recents");
            }
            txn.commit().expect("commit");
        }

        // Flush an insert carrying an OLDER recorded-at, as the insert queue would.
        let pending = HashMap::from([(
            "k".to_owned(),
            PendingInsert {
                bytes: vec![1, 2, 3],
                recorded_at_millis: 1_000,
            },
        )]);
        write_pending_inserts(&db, &pending).expect("insert flush should succeed");

        let stored = {
            let txn = db.begin_read().expect("read txn");
            let recents = txn.open_table(RECENTS_TABLE).expect("recents table");
            recents.get("k").expect("get").expect("present").value()
        };

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            stored, 2_000,
            "insert flush must not lower a newer recents timestamp"
        );
    }
}
