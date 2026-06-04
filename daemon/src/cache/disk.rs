use crate::{
    cache::key::{
        ANALYZER_VERSION, CacheIdentityV3, FileFingerprint, cache_key_matches_package,
        decode_cache_identity, fingerprints_are_current,
    },
    cache::memory::CachedImport,
    ipc::protocol::{ImportResult, ModuleContribution},
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const RECENTS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("cache_recents");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 3;
const RECENCY_TOUCH_FLUSH_BATCH: usize = 64;

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

    pub fn insert(&self, key: &str, cached: &CachedImport) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        let mut persisted = cached.clone();
        persisted.result.cache_hit = false;

        let envelope = cache_envelope(key, persisted);
        let bytes = match rmp_serde::to_vec(&envelope) {
            Ok(b) => b,
            Err(_) => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                let _ = table.insert(key, bytes.as_slice());
            }
            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                let _ = recents.insert(key, unix_millis_now());
            }
            let _ = write_txn.commit();
        }
        self.remove_pending_touch(key);
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
        let pending_touches = match self.pending_touches.lock() {
            Ok(mut pending_touches) => {
                if pending_touches.is_empty() {
                    return;
                }
                std::mem::take(&mut *pending_touches)
            }
            Err(_) => return,
        };
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                for (key, timestamp) in pending_touches {
                    let _ = recents.insert(key.as_str(), timestamp);
                }
            }
            let _ = write_txn.commit();
        }
    }

    pub fn pending_touch_len(&self) -> usize {
        self.pending_touches
            .lock()
            .map(|pending_touches| pending_touches.len())
            .unwrap_or(0)
    }

    pub fn remove(&self, key: &str) {
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

        keys.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        keys.truncate(limit);
        keys.into_iter().map(|(key, _)| key).collect()
    }

    pub fn invalidate_package(&self, package_name: &str) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                let mut keys_to_remove = Vec::new();

                if let Ok(iter) = table.iter() {
                    for result in iter {
                        if let Ok((key, _)) = result
                            && cache_key_matches_package(key.value(), package_name)
                        {
                            keys_to_remove.push(key.value().to_owned());
                        }
                    }
                }

                for key in keys_to_remove {
                    let _ = table.remove(key.as_str());
                    if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                        let _ = recents.remove(key.as_str());
                    }
                    self.remove_pending_touch(&key);
                }
            }
            let _ = write_txn.commit();
        }
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
    }

    fn disabled() -> Self {
        Self {
            db: None,
            pending_touches: Mutex::new(HashMap::new()),
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
        self.flush_pending_touches();
    }
}

fn unix_millis_now() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
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
            });
        }
        return None;
    }

    rmp_serde::from_slice::<ImportResult>(bytes)
        .ok()
        .map(|result| CachedImport {
            result,
            dependency_fingerprints: Vec::new(),
        })
}

fn cache_warn(message: String) {
    eprintln!("[import-lens-daemon] cache warning: {message}");
}
