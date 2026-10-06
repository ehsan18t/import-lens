//! Bounded index of real paths loaded by successful engine builds. It feeds the first-party
//! file-size freshness signal without retaining any linker or AST state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{cache::key::path_is_definitely_gone, ipc::protocol::ImportRuntime};

const MAX_DEPENDENCY_PATH_SETS: usize = 32;
type DependencyKey = (PathBuf, ImportRuntime);

static INDEX: LazyLock<DependencyPathIndex> =
    LazyLock::new(|| DependencyPathIndex::new(MAX_DEPENDENCY_PATH_SETS));

struct PathSet {
    paths: Vec<PathBuf>,
    /// Bumped by reads under the shared lock, so a lookup never takes the write lock.
    last_used: AtomicU64,
}

/// Least-recently-used: the file-size poll reads a first-party set on every request, so a
/// set in use is never the victim of a build recording another package's paths. A missing
/// set silently degrades that file's freshness token to the entry stat alone.
struct DependencyPathIndex {
    sets: RwLock<HashMap<DependencyKey, PathSet>>,
    clock: AtomicU64,
    capacity: usize,
}

impl DependencyPathIndex {
    fn new(capacity: usize) -> Self {
        Self {
            sets: RwLock::new(HashMap::new()),
            clock: AtomicU64::new(0),
            capacity,
        }
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// Poison-tolerant, like every other shared map in the daemon. The release build unwinds
    /// so a panicking file can be isolated; an `.expect()` here would turn one contained
    /// panic into a daemon that panics on every later analysis. A poisoned index costs at
    /// worst a stale path set, re-recorded on the next build.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<DependencyKey, PathSet>> {
        self.sets
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<DependencyKey, PathSet>> {
        self.sets
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(&self, key: DependencyKey, mut paths: Vec<PathBuf>) {
        paths.sort();
        paths.dedup();
        let last_used = AtomicU64::new(self.tick());

        let mut sets = self.write();
        if sets.len() >= self.capacity
            && !sets.contains_key(&key)
            && let Some(victim) = sets
                .iter()
                .min_by_key(|(_, set)| set.last_used.load(Ordering::Relaxed))
                .map(|(key, _)| key.clone())
        {
            sets.remove(&victim);
        }
        sets.insert(key, PathSet { paths, last_used });
    }

    fn get(&self, key: &DependencyKey) -> Option<Vec<PathBuf>> {
        let sets = self.read();
        let set = sets.get(key)?;
        set.last_used.store(self.tick(), Ordering::Relaxed);
        Some(set.paths.clone())
    }
}

pub(crate) fn record_loaded_paths(
    entry_path: PathBuf,
    runtime: ImportRuntime,
    loaded_paths: Vec<PathBuf>,
) {
    INDEX.record((entry_path, runtime), loaded_paths);
}

pub(crate) fn cached_loaded_paths(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Option<Vec<PathBuf>> {
    INDEX.get(&(entry_path.to_path_buf(), runtime))
}

pub(crate) fn clear() {
    INDEX.write().clear();
}

pub(crate) fn invalidate_package(package_name: &str) {
    let package_segment = format!("node_modules/{package_name}/");
    INDEX.write().retain(|(entry_path, _), _| {
        !entry_path
            .to_string_lossy()
            .replace('\\', "/")
            .contains(&package_segment)
    });
}

pub(crate) fn purge_missing() -> usize {
    let mut sets = INDEX.write();
    let before = sets.len();
    sets.retain(|(entry_path, _), _| !path_is_definitely_gone(entry_path));
    before - sets.len()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::ipc::protocol::ImportRuntime;

    use super::{DependencyPathIndex, cached_loaded_paths, clear, record_loaded_paths};

    #[test]
    fn records_sorted_deduplicated_paths_by_runtime() {
        clear();
        let entry = PathBuf::from("/pkg/index.js");
        record_loaded_paths(
            entry.clone(),
            ImportRuntime::Client,
            vec![
                PathBuf::from("/pkg/z.js"),
                PathBuf::from("/pkg/a.js"),
                PathBuf::from("/pkg/a.js"),
            ],
        );

        assert_eq!(
            cached_loaded_paths(&entry, ImportRuntime::Client),
            Some(vec![PathBuf::from("/pkg/a.js"), PathBuf::from("/pkg/z.js")])
        );
        assert_eq!(cached_loaded_paths(&entry, ImportRuntime::Server), None);
        clear();
    }

    /// A set the file-size poll keeps reading must survive any number of builds recording
    /// other packages; only the least recently used set is evicted.
    #[test]
    fn eviction_spares_the_sets_in_use() {
        let index = DependencyPathIndex::new(32);
        let key = |name: &str| {
            (
                PathBuf::from(format!("/ws/{name}/index.js")),
                ImportRuntime::Component,
            )
        };
        let hot = ["a", "b", "c", "d"].map(key);
        for hot_key in &hot {
            index.record(hot_key.clone(), vec![hot_key.0.clone()]);
        }
        for cold in 0..28 {
            index.record(key(&format!("node_modules/cold{cold}")), Vec::new());
        }

        for round in 0..200 {
            for hot_key in &hot {
                assert!(
                    index.get(hot_key).is_some(),
                    "round {round}: {hot_key:?} was evicted"
                );
            }
            index.record(key(&format!("node_modules/new{round}")), Vec::new());
        }

        assert_eq!(index.read().len(), 32, "the index stays bounded");
        assert!(
            index.get(&key("node_modules/new199")).is_some(),
            "the most recent record is kept"
        );
        assert!(
            index.get(&key("node_modules/cold0")).is_none(),
            "the least recently used set is the victim"
        );
    }
}
