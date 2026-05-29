use crate::{cache::disk::DiskCache, ipc::protocol::ImportResult};
use papaya::HashMap;
use std::path::PathBuf;

#[derive(Debug)]
pub struct ImportCache {
    memory: HashMap<String, ImportResult>,
    disk: DiskCache,
}

impl Default for ImportCache {
    fn default() -> Self {
        Self {
            memory: HashMap::new(),
            disk: DiskCache::default(),
        }
    }
}

impl ImportCache {
    pub fn new(storage_path: Option<PathBuf>, enable_disk_cache: bool) -> Self {
        Self {
            memory: HashMap::new(),
            disk: DiskCache::new(storage_path, enable_disk_cache),
        }
    }

    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let memory = self.memory.pin();
        if let Some(result) = memory.get(key) {
            let mut result = result.clone();
            result.cache_hit = true;
            return Some(result);
        }

        if let Some(mut result) = self.disk.get(key) {
            memory.insert(key.to_owned(), result.clone());
            result.cache_hit = true;
            return Some(result);
        }

        None
    }

    pub fn insert(&self, key: String, result: ImportResult) {
        self.disk.insert(&key, &result);
        self.memory.pin().insert(key, result);
    }

    pub fn invalidate_package(&self, package_name: &str) {
        self.disk.invalidate_package(package_name);

        let memory = self.memory.pin();
        let root_prefix = format!("{package_name}@");
        let subpath_prefix = format!("{package_name}/");
        let keys = memory
            .iter()
            .filter_map(|(key, _)| {
                (key.starts_with(&root_prefix) || key.starts_with(&subpath_prefix))
                    .then(|| key.clone())
            })
            .collect::<Vec<_>>();

        for key in keys {
            memory.remove(&key);
        }
    }

    pub fn clear(&self) {
        self.disk.clear();
        self.memory.pin().clear();
    }
}
