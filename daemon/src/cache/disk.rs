use crate::ipc::protocol::ImportResult;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::PathBuf;

const IMPORTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("imports");

#[derive(Debug)]
pub struct DiskCache {
    db: Option<Database>,
}

impl Default for DiskCache {
    fn default() -> Self {
        Self { db: None }
    }
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

        let db_path = storage_path.join("import-lens-cache.redb");
        let db = Database::create(&db_path).ok();

        if let Some(db) = &db {
            if let Ok(write_txn) = db.begin_write() {
                let _ = write_txn.open_table(IMPORTS_TABLE);
                let _ = write_txn.commit();
            }
        }

        Self { db }
    }

    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let db = self.db.as_ref()?;
        let read_txn = db.begin_read().ok()?;
        let table = read_txn.open_table(IMPORTS_TABLE).ok()?;
        let value = table.get(key).ok()??;

        let bytes = value.value();
        let mut result: ImportResult = rmp_serde::from_slice(bytes).ok()?;
        result.cache_hit = true;
        Some(result)
    }

    pub fn insert(&self, key: &str, result: &ImportResult) {
        let db = match self.db.as_ref() {
            Some(db) => db,
            None => return,
        };

        let bytes = match rmp_serde::to_vec(result) {
            Ok(b) => b,
            Err(_) => return,
        };

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(IMPORTS_TABLE) {
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
        let subpath_prefix = format!("{package_name}/");

        if let Ok(write_txn) = db.begin_write() {
            if let Ok(mut table) = write_txn.open_table(IMPORTS_TABLE) {
                let mut keys_to_remove = Vec::new();

                if let Ok(iter) = table.iter() {
                    for result in iter {
                        if let Ok((key, _)) = result {
                            let key_str = key.value();
                            if key_str.starts_with(&root_prefix)
                                || key_str.starts_with(&subpath_prefix)
                            {
                                keys_to_remove.push(key_str.to_owned());
                            }
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
            if let Ok(mut table) = write_txn.open_table(IMPORTS_TABLE) {
                let mut keys_to_remove = Vec::new();
                if let Ok(iter) = table.iter() {
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
}
