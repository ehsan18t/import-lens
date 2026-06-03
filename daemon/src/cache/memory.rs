use crate::{
    cache::{
        disk::DiskCache,
        key::{FileFingerprint, cache_key_matches_package, fingerprints_are_current},
    },
    ipc::protocol::ImportResult,
};
use papaya::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct CachedImport {
    pub result: ImportResult,
    pub dependency_fingerprints: Vec<FileFingerprint>,
}

#[derive(Debug)]
pub struct ImportCache {
    memory: HashMap<String, CachedImport>,
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
            for (key, cached) in disk.load_all() {
                pinned.insert(key, cached);
            }
        }

        Self { memory, disk }
    }

    pub fn get(&self, key: &str) -> Option<ImportResult> {
        let memory = self.memory.pin();
        if let Some(cached) = memory.get(key) {
            if !fingerprints_are_current(&cached.dependency_fingerprints) {
                memory.remove(key);
                self.disk.remove(key);
                return None;
            }
            let mut result = cached.result.clone();
            result.cache_hit = true;
            self.disk.touch(key);
            return Some(result);
        }

        if let Some(cached) = self.disk.get(key) {
            let mut result = cached.result.clone();
            memory.insert(key.to_owned(), cached);
            result.cache_hit = true;
            return Some(result);
        }

        None
    }

    pub fn insert(&self, key: String, result: ImportResult) {
        self.insert_with_fingerprints(key, result, Vec::new());
    }

    pub fn insert_with_fingerprints(
        &self,
        key: String,
        result: ImportResult,
        dependency_fingerprints: Vec<FileFingerprint>,
    ) {
        let cached = CachedImport {
            result,
            dependency_fingerprints,
        };
        self.disk.insert(&key, &cached);
        self.memory.pin().insert(key, cached);
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
