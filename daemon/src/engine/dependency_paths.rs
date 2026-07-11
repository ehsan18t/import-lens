//! Bounded index of real paths loaded by successful engine builds.
//!
//! This preserves the first-party file-size freshness signal previously
//! supplied incidentally by the custom module-graph cache without retaining
//! any linker or AST state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use crate::{cache::key::path_is_definitely_gone, ipc::protocol::ImportRuntime};

const MAX_DEPENDENCY_PATH_SETS: usize = 32;
type DependencyKey = (PathBuf, ImportRuntime);

static DEPENDENCY_PATHS: OnceLock<RwLock<HashMap<DependencyKey, Vec<PathBuf>>>> = OnceLock::new();

fn index() -> &'static RwLock<HashMap<DependencyKey, Vec<PathBuf>>> {
    DEPENDENCY_PATHS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn record_loaded_paths(
    entry_path: PathBuf,
    runtime: ImportRuntime,
    mut loaded_paths: Vec<PathBuf>,
) {
    loaded_paths.sort();
    loaded_paths.dedup();

    let mut index = index()
        .write()
        .expect("dependency path index should not be poisoned");
    // Bounded, not LRU: HashMap iteration order makes this an arbitrary
    // victim, which is acceptable because a dropped set only costs one
    // re-record on the next successful build.
    if index.len() >= MAX_DEPENDENCY_PATH_SETS
        && !index.contains_key(&(entry_path.clone(), runtime))
        && let Some(victim) = index.keys().next().cloned()
    {
        index.remove(&victim);
    }
    index.insert((entry_path, runtime), loaded_paths);
}

pub(crate) fn cached_loaded_paths(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Option<Vec<PathBuf>> {
    index()
        .read()
        .expect("dependency path index should not be poisoned")
        .get(&(entry_path.to_path_buf(), runtime))
        .cloned()
}

pub(crate) fn clear() {
    index()
        .write()
        .expect("dependency path index should not be poisoned")
        .clear();
}

pub(crate) fn invalidate_package(package_name: &str) {
    let package_segment = format!("node_modules/{package_name}/");
    index()
        .write()
        .expect("dependency path index should not be poisoned")
        .retain(|(entry_path, _), _| {
            !entry_path
                .to_string_lossy()
                .replace('\\', "/")
                .contains(&package_segment)
        });
}

pub(crate) fn purge_missing() -> usize {
    let mut index = index()
        .write()
        .expect("dependency path index should not be poisoned");
    let before = index.len();
    index.retain(|(entry_path, _), _| !path_is_definitely_gone(entry_path));
    before - index.len()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::ipc::protocol::ImportRuntime;

    use super::{cached_loaded_paths, clear, record_loaded_paths};

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
}
