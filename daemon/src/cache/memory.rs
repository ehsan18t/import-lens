use crate::{
    cache::{disk::DiskCache, key::cache_key_matches_package},
    ipc::protocol::ImportResult,
};
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
        let memory = HashMap::new();
        let disk = DiskCache::new(storage_path, enable_disk_cache);

        {
            let pinned = memory.pin();
            for (key, result) in disk.load_all() {
                pinned.insert(key, result);
            }
        }

        Self { memory, disk }
    }

    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let memory = self.memory.pin();
        if let Some(result) = memory.get(key) {
            let mut result = result.clone();
            result.cache_hit = true;
            self.disk.touch(key);
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
        let keys = memory
            .iter()
            .filter(|(key, _)| cache_key_matches_package(key, package_name))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        for key in keys {
            memory.remove(&key);
        }
    }

    pub fn clear(&self) {
        self.disk.clear();
        self.memory.pin().clear();
    }

    pub fn memory_len(&self) -> usize {
        self.memory.pin().len()
    }

    pub fn recent_keys(&self, limit: usize) -> Vec<String> {
        self.disk.recent_keys(limit)
    }
}
