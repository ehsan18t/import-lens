use crate::{
    cache::memory::ImportCache,
    ipc::protocol::{CacheOperationResult, CacheShardInfo},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const SHARD_METADATA_FILE_NAME: &str = "importlens-project-cache.json";
const LEGACY_CENTRAL_CACHE_DB_FILE_NAME: &str = "importlens.redb";
const LEGACY_CENTRAL_CACHE_SHARD_ID: &str = "legacy-central";
const PROJECT_METADATA_WRITE_INTERVAL_MILLIS: u64 = 60_000;

#[derive(Debug)]
pub struct ProjectCacheRegistry {
    base_path: Option<PathBuf>,
    enable_disk_cache: bool,
    max_size_mb: u64,
    max_age_days: u64,
    loaded: Mutex<HashMap<String, LoadedProjectCache>>,
    last_cleanup_millis: Mutex<Option<u64>>,
}

#[derive(Debug, Clone)]
struct LoadedProjectCache {
    project_root: String,
    normalized_root: String,
    cache_path: PathBuf,
    cache: Arc<ImportCache>,
    last_used_millis: u64,
    last_metadata_write_millis: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectCacheMetadata {
    shard_id: String,
    project_root: String,
    normalized_root: String,
    last_used_millis: u64,
}

impl ProjectCacheRegistry {
    pub fn new(
        base_path: Option<PathBuf>,
        enable_disk_cache: bool,
        max_size_mb: u64,
        max_age_days: u64,
    ) -> Self {
        Self {
            base_path,
            enable_disk_cache,
            max_size_mb,
            max_age_days,
            loaded: Mutex::new(HashMap::new()),
            last_cleanup_millis: Mutex::new(None),
        }
    }

    pub fn cache_for_root(&self, project_root: &Path) -> Arc<ImportCache> {
        let shard_id = project_cache_shard_id(project_root);
        let now = unix_millis_now();

        if let Ok(mut loaded) = self.loaded.lock() {
            if let Some(shard) = loaded.get_mut(&shard_id) {
                shard.last_used_millis = now;
                if should_write_project_metadata(shard.last_metadata_write_millis, now) {
                    self.write_metadata_for_loaded(&shard_id, shard);
                    shard.last_metadata_write_millis = now;
                }
                return Arc::clone(&shard.cache);
            }

            let normalized_root = normalize_project_root(project_root);
            let cache_path = self.cache_path_for_shard(&shard_id);
            let disk_path = self.disk_cache_path(&cache_path);
            let cache = Arc::new(ImportCache::new(disk_path, self.enable_disk_cache));
            let shard = LoadedProjectCache {
                project_root: project_root.to_string_lossy().to_string(),
                normalized_root,
                cache_path,
                cache: Arc::clone(&cache),
                last_used_millis: now,
                last_metadata_write_millis: now,
            };
            self.write_metadata_for_loaded(&shard_id, &shard);
            loaded.insert(shard_id, shard);
            return cache;
        }

        Arc::new(ImportCache::new(None, false))
    }

    pub fn list_shards(&self) -> Vec<CacheShardInfo> {
        let mut shards = self.scan_disk_shards();

        if let Ok(loaded) = self.loaded.lock() {
            for (shard_id, shard) in loaded.iter() {
                let info = self.info_for_loaded(shard_id, shard);
                if let Some(existing) = shards
                    .iter_mut()
                    .find(|candidate| candidate.shard_id == *shard_id)
                {
                    *existing = info;
                } else {
                    shards.push(info);
                }
            }
        }

        shards.sort_by(|left, right| {
            right
                .size_bytes
                .cmp(&left.size_bytes)
                .then_with(|| left.project_root.cmp(&right.project_root))
        });
        shards
    }

    pub fn status_for_root(&self, project_root: Option<&Path>) -> ProjectCacheStatus {
        let shards = self.list_shards();
        let total_size_bytes = shards.iter().map(|shard| shard.size_bytes).sum();
        let normalized_root = project_root.map(normalize_project_root);
        let current_project = normalized_root.and_then(|root| {
            shards
                .iter()
                .find(|shard| shard.normalized_root == root)
                .cloned()
        });
        let last_cleanup_millis = self
            .last_cleanup_millis
            .lock()
            .map(|last_cleanup| *last_cleanup)
            .unwrap_or(None);

        ProjectCacheStatus {
            total_size_bytes,
            project_count: shards.len(),
            max_size_mb: self.max_size_mb,
            max_age_days: self.max_age_days,
            last_cleanup_millis,
            current_project,
        }
    }

    pub fn cleanup(&self) -> ProjectCacheCleanup {
        let now = unix_millis_now();
        let max_age_millis = self.max_age_days.saturating_mul(24 * 60 * 60 * 1000);
        let max_size_bytes = self.max_size_mb.saturating_mul(1024 * 1024);
        let mut removed = Vec::new();
        let mut failed = Vec::new();
        let mut removed_ids = HashSet::new();

        for shard in self.list_shards() {
            let expired = shard
                .last_used_millis
                .is_some_and(|last_used| now.saturating_sub(last_used) > max_age_millis);

            if expired {
                let result = self.remove_shard_by_id(&shard.shard_id);
                removed_ids.insert(shard.shard_id);
                push_operation_result(result, &mut removed, &mut failed);
            }
        }

        let mut remaining = self
            .list_shards()
            .into_iter()
            .filter(|shard| !removed_ids.contains(&shard.shard_id))
            .collect::<Vec<_>>();
        let mut total_size_bytes = remaining.iter().map(|shard| shard.size_bytes).sum::<u64>();

        if max_size_bytes > 0 && total_size_bytes > max_size_bytes {
            remaining.sort_by(|left, right| {
                left.last_used_millis
                    .unwrap_or(0)
                    .cmp(&right.last_used_millis.unwrap_or(0))
            });

            for shard in remaining {
                if total_size_bytes <= max_size_bytes {
                    break;
                }

                let size_bytes = shard.size_bytes;
                let result = self.remove_shard_by_id(&shard.shard_id);
                if result.removed {
                    total_size_bytes = total_size_bytes.saturating_sub(size_bytes);
                }
                push_operation_result(result, &mut removed, &mut failed);
            }
        }

        if let Ok(mut last_cleanup) = self.last_cleanup_millis.lock() {
            *last_cleanup = Some(now);
        }

        ProjectCacheCleanup {
            total_size_bytes: self.total_size_bytes(),
            removed,
            failed,
        }
    }

    pub fn remove_current_project(&self, project_root: &Path) -> Vec<CacheOperationResult> {
        vec![self.remove_shard_by_id(&project_cache_shard_id(project_root))]
    }

    pub fn remove_selected(&self, shard_ids: &[String]) -> Vec<CacheOperationResult> {
        shard_ids
            .iter()
            .map(|shard_id| self.remove_shard_by_id(shard_id))
            .collect()
    }

    pub fn remove_all(&self) -> Vec<CacheOperationResult> {
        let mut shard_ids = self
            .list_shards()
            .into_iter()
            .map(|shard| shard.shard_id)
            .collect::<Vec<_>>();
        shard_ids.sort();
        shard_ids.dedup();
        shard_ids
            .iter()
            .map(|shard_id| self.remove_shard_by_id(shard_id))
            .collect()
    }

    pub fn invalidate_package(&self, package_name: &str) {
        let loaded_ids = self
            .loaded
            .lock()
            .map(|loaded| {
                for shard in loaded.values() {
                    shard.cache.invalidate_package(package_name);
                }

                loaded.keys().cloned().collect::<HashSet<_>>()
            })
            .unwrap_or_default();

        for shard in self
            .scan_disk_shards()
            .into_iter()
            .filter(|shard| !loaded_ids.contains(&shard.shard_id))
        {
            let cache_path = PathBuf::from(shard.cache_path);
            if cache_path.as_os_str().is_empty() {
                continue;
            }
            ImportCache::new_with_recent_preload_limit(Some(cache_path), self.enable_disk_cache, 0)
                .invalidate_package(package_name);
        }
    }

    pub fn clear_all(&self) {
        let _ = self.remove_all();
    }

    pub fn memory_len(&self) -> usize {
        if let Ok(loaded) = self.loaded.lock() {
            return loaded.values().map(|shard| shard.cache.memory_len()).sum();
        }

        0
    }

    pub fn recent_keys(&self, project_root: &Path, limit: usize) -> Vec<String> {
        self.cache_for_root(project_root).recent_keys(limit)
    }

    pub fn flush_to_disk(&self) -> Result<(), String> {
        let caches = self
            .loaded
            .lock()
            .map(|loaded| {
                loaded
                    .values()
                    .map(|shard| Arc::clone(&shard.cache))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for cache in caches {
            cache.flush_to_disk()?;
        }

        Ok(())
    }

    fn total_size_bytes(&self) -> u64 {
        self.list_shards()
            .iter()
            .map(|shard| shard.size_bytes)
            .sum()
    }

    fn remove_shard_by_id(&self, shard_id: &str) -> CacheOperationResult {
        let loaded = self
            .loaded
            .lock()
            .ok()
            .and_then(|mut loaded| loaded.remove(shard_id));
        let metadata = loaded
            .as_ref()
            .map(|shard| ProjectCacheMetadata {
                shard_id: shard_id.to_owned(),
                project_root: shard.project_root.clone(),
                normalized_root: shard.normalized_root.clone(),
                last_used_millis: shard.last_used_millis,
            })
            .or_else(|| self.read_metadata_for_shard(shard_id));
        let cache_path = loaded
            .as_ref()
            .map(|shard| shard.cache_path.clone())
            .unwrap_or_else(|| self.cache_path_for_shard(shard_id));

        if let Some(shard) = loaded {
            shard.cache.clear();
        }

        let project_root = metadata
            .as_ref()
            .map(|metadata| metadata.project_root.clone())
            .unwrap_or_default();
        let cache_path_text = cache_path.to_string_lossy().to_string();

        if metadata.is_none() && !cache_path.exists() {
            return CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: false,
                error: Some("cache shard not found".to_owned()),
            };
        }

        if cache_path.as_os_str().is_empty() {
            return CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: true,
                error: None,
            };
        }

        match fs::remove_dir_all(&cache_path) {
            Ok(()) => CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: true,
                error: None,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: true,
                error: None,
            },
            Err(error) => CacheOperationResult {
                shard_id: shard_id.to_owned(),
                project_root,
                cache_path: cache_path_text,
                removed: false,
                error: Some(error.to_string()),
            },
        }
    }

    fn info_for_loaded(&self, shard_id: &str, shard: &LoadedProjectCache) -> CacheShardInfo {
        CacheShardInfo {
            shard_id: shard_id.to_owned(),
            project_root: shard.project_root.clone(),
            normalized_root: shard.normalized_root.clone(),
            cache_path: shard.cache_path.to_string_lossy().to_string(),
            size_bytes: directory_size(&shard.cache_path),
            last_used_millis: Some(shard.last_used_millis),
            loaded: true,
        }
    }

    fn scan_disk_shards(&self) -> Vec<CacheShardInfo> {
        let Some(base_path) = self.base_path.as_ref() else {
            return Vec::new();
        };

        let entries = match fs::read_dir(base_path) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let cache_path = entry.path();
                if !cache_path.is_dir() {
                    return None;
                }
                let metadata_path = cache_path.join(SHARD_METADATA_FILE_NAME);
                let metadata = read_metadata(&metadata_path)?;

                Some(CacheShardInfo {
                    shard_id: metadata.shard_id,
                    project_root: metadata.project_root,
                    normalized_root: metadata.normalized_root,
                    cache_path: cache_path.to_string_lossy().to_string(),
                    size_bytes: directory_size(&cache_path),
                    last_used_millis: Some(metadata.last_used_millis),
                    loaded: false,
                })
            })
            .collect()
    }

    fn write_metadata_for_loaded(&self, shard_id: &str, shard: &LoadedProjectCache) {
        if !self.storage_enabled() {
            return;
        }

        let metadata = ProjectCacheMetadata {
            shard_id: shard_id.to_owned(),
            project_root: shard.project_root.clone(),
            normalized_root: shard.normalized_root.clone(),
            last_used_millis: shard.last_used_millis,
        };
        let _ = write_metadata(&shard.cache_path.join(SHARD_METADATA_FILE_NAME), &metadata);
    }

    fn read_metadata_for_shard(&self, shard_id: &str) -> Option<ProjectCacheMetadata> {
        let cache_path = self.cache_path_for_shard(shard_id);
        read_metadata(&cache_path.join(SHARD_METADATA_FILE_NAME))
    }

    fn disk_cache_path(&self, cache_path: &Path) -> Option<PathBuf> {
        self.storage_enabled().then(|| cache_path.to_path_buf())
    }

    fn cache_path_for_shard(&self, shard_id: &str) -> PathBuf {
        self.base_path
            .as_ref()
            .filter(|_| self.storage_enabled())
            .map(|base_path| base_path.join(shard_id))
            .unwrap_or_default()
    }

    fn storage_enabled(&self) -> bool {
        self.enable_disk_cache && self.base_path.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCacheStatus {
    pub total_size_bytes: u64,
    pub project_count: usize,
    pub max_size_mb: u64,
    pub max_age_days: u64,
    pub last_cleanup_millis: Option<u64>,
    pub current_project: Option<CacheShardInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCacheCleanup {
    pub total_size_bytes: u64,
    pub removed: Vec<CacheOperationResult>,
    pub failed: Vec<CacheOperationResult>,
}

pub fn normalize_project_root(project_root: &Path) -> String {
    let raw = project_root.to_string_lossy().replace('\\', "/");
    let trimmed = raw.trim_end_matches('/').to_owned();

    if cfg!(windows) || trimmed.as_bytes().get(1).is_some_and(|byte| *byte == b':') {
        return trimmed.to_ascii_lowercase();
    }

    trimmed
}

pub fn project_cache_shard_id(project_root: &Path) -> String {
    let normalized = normalize_project_root(project_root);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;

    for byte in normalized.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    format!("v1-{hash:016x}")
}

pub fn remove_legacy_central_cache(storage_path: &Path) -> Option<CacheOperationResult> {
    let cache_path = storage_path.join(LEGACY_CENTRAL_CACHE_DB_FILE_NAME);

    if !cache_path.exists() {
        return None;
    }

    let cache_path_text = cache_path.to_string_lossy().to_string();
    let result = match fs::remove_file(&cache_path) {
        Ok(()) => CacheOperationResult {
            shard_id: LEGACY_CENTRAL_CACHE_SHARD_ID.to_owned(),
            project_root: String::new(),
            cache_path: cache_path_text,
            removed: true,
            error: None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CacheOperationResult {
            shard_id: LEGACY_CENTRAL_CACHE_SHARD_ID.to_owned(),
            project_root: String::new(),
            cache_path: cache_path_text,
            removed: true,
            error: None,
        },
        Err(error) => CacheOperationResult {
            shard_id: LEGACY_CENTRAL_CACHE_SHARD_ID.to_owned(),
            project_root: String::new(),
            cache_path: cache_path_text,
            removed: false,
            error: Some(error.to_string()),
        },
    };

    Some(result)
}

fn push_operation_result(
    result: CacheOperationResult,
    removed: &mut Vec<CacheOperationResult>,
    failed: &mut Vec<CacheOperationResult>,
) {
    if result.removed {
        removed.push(result);
    } else {
        failed.push(result);
    }
}

fn should_write_project_metadata(last_write_millis: u64, now_millis: u64) -> bool {
    now_millis.saturating_sub(last_write_millis) >= PROJECT_METADATA_WRITE_INTERVAL_MILLIS
}

fn read_metadata(path: &Path) -> Option<ProjectCacheMetadata> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_metadata(path: &Path, metadata: &ProjectCacheMetadata) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create cache metadata directory: {error}"))?;
    }

    let contents = serde_json::to_string(metadata)
        .map_err(|error| format!("failed to serialize cache metadata: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("failed to write cache metadata: {error}"))
}

fn directory_size(path: &Path) -> u64 {
    if path.as_os_str().is_empty() {
        return 0;
    }

    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };

    if metadata.is_file() {
        return metadata.len();
    }

    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

fn unix_millis_now() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}
