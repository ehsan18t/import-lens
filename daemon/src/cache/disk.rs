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
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const RECENTS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("cache_recents");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 3;

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
}

impl DiskCache {
    pub fn new(storage_path: Option<PathBuf>, enabled: bool) -> Self {
        if !enabled {
            return Self { db: None };
        }

        let storage_path = match storage_path {
            Some(path) => path,
            None => return Self { db: None },
        };

        Self {
            db: Self::open_database(&storage_path),
        }
    }

    pub fn get(&self, key: &str) -> Option<CachedImport> {
        let db = self.db.as_ref()?;
        let read_txn = db.begin_read().ok()?;
        let table = read_txn.open_table(CACHE_TABLE).ok()?;
        let value = table.get(key).ok()??;

        let bytes = value.value();
        let cached = decode_cached_result(bytes)?;
        if !fingerprints_are_current(&cached.dependency_fingerprints) {
            drop(table);
            drop(read_txn);
            self.remove(key);
            return None;
        }
        self.touch(key);
        Some(cached)
    }

    pub fn load_all(&self) -> Vec<(String, CachedImport)> {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return Vec::new(),
        };

        let read_txn = match db.begin_read() {
            Ok(txn) => txn,
            Err(error) => {
                cache_warn(format!("failed to begin cache preload read: {error}"));
                return Vec::new();
            }
        };
        let table = match read_txn.open_table(CACHE_TABLE) {
            Ok(table) => table,
            Err(error) => {
                cache_warn(format!("failed to open cache table for preload: {error}"));
                return Vec::new();
            }
        };
        let iter = match table.iter() {
            Ok(iter) => iter,
            Err(error) => {
                cache_warn(format!(
                    "failed to iterate cache table for preload: {error}"
                ));
                return Vec::new();
            }
        };

        iter.filter_map(|entry| {
            let (key, value) = entry.ok()?;
            let cached = decode_cached_result(value.value())?;
            if !fingerprints_are_current(&cached.dependency_fingerprints) {
                return None;
            }
            Some((key.value().to_owned(), cached))
        })
        .collect()
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
    }

    pub fn touch(&self, key: &str) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut recents) = write_txn.open_table(RECENTS_TABLE) {
                let _ = recents.insert(key, unix_millis_now());
            }
            let _ = write_txn.commit();
        }
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

        match Self::ensure_schema(&db) {
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

        if let Err(error) = Self::ensure_schema(&db) {
            cache_warn(format!(
                "failed to initialize cache database {}: {error}",
                db_path.display()
            ));
            return None;
        }

        Some(db)
    }

    fn ensure_schema(db: &Database) -> Result<(), String> {
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
                None => {
                    metadata
                        .insert(SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION)
                        .map_err(|error| format!("failed to write schema version: {error}"))?;
                    CURRENT_SCHEMA_VERSION
                }
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
        full_contributions: cached.result.module_breakdown.clone().unwrap_or_default(),
        result: cached.result,
        package_identity,
        dependency_fingerprints,
    }
}

fn decode_cached_result(bytes: &[u8]) -> Option<CachedImport> {
    if let Ok(envelope) = rmp_serde::from_slice::<CacheEnvelope>(bytes) {
        if envelope.analyzer_version == ANALYZER_VERSION {
            return Some(CachedImport {
                result: envelope.result,
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
