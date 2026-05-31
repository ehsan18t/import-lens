use crate::ipc::protocol::ImportResult;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::{
    fs,
    path::{Path, PathBuf},
};

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const SCHEMA_VERSION_KEY: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u64 = 1;

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

    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let db = self.db.as_ref()?;
        let read_txn = db.begin_read().ok()?;
        let table = read_txn.open_table(CACHE_TABLE).ok()?;
        let value = table.get(key).ok()??;

        let bytes = value.value();
        let mut result: ImportResult = rmp_serde::from_slice(bytes).ok()?;
        result.cache_hit = true;
        Some(result)
    }

    pub fn load_all(&self) -> Vec<(String, ImportResult)> {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return Vec::new(),
        };

        let read_txn = match db.begin_read() {
            Ok(txn) => txn,
            Err(error) => {
                CacheLogger::warn(format!("failed to begin cache preload read: {error}"));
                return Vec::new();
            }
        };
        let table = match read_txn.open_table(CACHE_TABLE) {
            Ok(table) => table,
            Err(error) => {
                CacheLogger::warn(format!("failed to open cache table for preload: {error}"));
                return Vec::new();
            }
        };
        let iter = match table.iter() {
            Ok(iter) => iter,
            Err(error) => {
                CacheLogger::warn(format!(
                    "failed to iterate cache table for preload: {error}"
                ));
                return Vec::new();
            }
        };

        iter.filter_map(|entry| {
            let (key, value) = entry.ok()?;
            let mut result = rmp_serde::from_slice::<ImportResult>(value.value()).ok()?;
            result.cache_hit = false;
            Some((key.value().to_owned(), result))
        })
        .collect()
    }

    pub fn insert(&self, key: &str, result: &ImportResult) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        let mut persisted = result.clone();
        persisted.cache_hit = false;

        let bytes = match rmp_serde::to_vec(&persisted) {
            Ok(b) => b,
            Err(_) => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                let _ = table.insert(key, bytes.as_slice());
            }
            let _ = write_txn.commit();
        }
    }

    pub fn invalidate_package(&self, package_name: &str) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        let root_prefix = format!("{package_name}@");
        let root_end = format!("{package_name}@\u{10FFFF}");
        let subpath_prefix = format!("{package_name}/");
        let subpath_end = format!("{package_name}/\u{10FFFF}");

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(CACHE_TABLE) {
                let mut keys_to_remove = Vec::new();

                if let Ok(iter) = table.range(root_prefix.as_str()..root_end.as_str()) {
                    for result in iter {
                        if let Ok((key, _)) = result {
                            keys_to_remove.push(key.value().to_owned());
                        }
                    }
                }

                if let Ok(iter) = table.range(subpath_prefix.as_str()..subpath_end.as_str()) {
                    for result in iter {
                        if let Ok((key, _)) = result {
                            keys_to_remove.push(key.value().to_owned());
                        }
                    }
                }

                for key in keys_to_remove {
                    let _ = table.remove(key.as_str());
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
            let _ = write_txn.commit();
        }
    }

    fn open_database(storage_path: &Path) -> Option<Database> {
        if let Err(error) = fs::create_dir_all(storage_path) {
            CacheLogger::warn(format!(
                "failed to create cache directory {}: {error}",
                storage_path.display()
            ));
            return None;
        }

        let db_path = storage_path.join(CACHE_DB_FILE_NAME);
        let db = match Database::create(&db_path) {
            Ok(db) => db,
            Err(error) => {
                CacheLogger::warn(format!(
                    "failed to open cache database {}: {error}",
                    db_path.display()
                ));
                return Self::recreate_database(&db_path);
            }
        };

        match Self::ensure_schema(&db) {
            Ok(()) => Some(db),
            Err(error) => {
                CacheLogger::warn(format!(
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
                CacheLogger::warn(format!(
                    "failed to delete cache database {}: {error}",
                    db_path.display()
                ));
            }
            return None;
        }

        let db = match Database::create(db_path) {
            Ok(db) => db,
            Err(error) => {
                CacheLogger::warn(format!(
                    "failed to recreate cache database {}: {error}",
                    db_path.display()
                ));
                return None;
            }
        };

        if let Err(error) = Self::ensure_schema(&db) {
            CacheLogger::warn(format!(
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

        write_txn
            .commit()
            .map_err(|error| format!("failed to commit schema transaction: {error}"))
    }
}

struct CacheLogger;

impl CacheLogger {
    fn warn(message: String) {
        eprintln!("[import-lens-daemon] cache warning: {message}");
    }
}
