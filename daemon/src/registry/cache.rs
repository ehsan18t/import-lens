use super::{
    constants::REGISTRY_CACHE_FILE_NAME,
    types::{RegistryPackageMetadata, RegistryPackageMetadataEntry},
};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

// Persist the full snapshot at most every N writes rather than on every write,
// so refreshing M packages does not rewrite the whole file M times (O(M^2)
// bytes). A trailing flush (per request / on Drop) persists the remainder.
const REGISTRY_PERSIST_BATCH: usize = 16;

#[derive(Debug)]
pub struct RegistryMetadataCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, RegistryPackageMetadataEntry>>,
    persist_lock: Mutex<()>,
    unpersisted_writes: AtomicUsize,
}

impl RegistryMetadataCache {
    pub fn new(storage_path: PathBuf) -> Self {
        let path = storage_path.join(REGISTRY_CACHE_FILE_NAME);
        let entries = load_entries(&path);
        Self {
            path,
            entries: Mutex::new(entries),
            persist_lock: Mutex::new(()),
            unpersisted_writes: AtomicUsize::new(0),
        }
    }

    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Mutex::new(HashMap::new()),
            persist_lock: Mutex::new(()),
            unpersisted_writes: AtomicUsize::new(0),
        }
    }

    pub fn get(&self, package_name: &str) -> Option<RegistryPackageMetadataEntry> {
        // Poisoned entries lock: degrade to a cache miss.
        self.entries
            .lock()
            .ok()?
            .get(&cache_key(package_name))
            .cloned()
    }

    pub fn write_entry(
        &self,
        package_name: &str,
        entry: RegistryPackageMetadataEntry,
    ) -> Result<(), String> {
        {
            let Ok(mut entries) = self.entries.lock() else {
                return Err("registry cache lock poisoned".to_owned());
            };
            entries.insert(cache_key(package_name), entry);
        }
        // The in-memory map is the source of truth (get reads it), so defer the
        // full-file persist; flush at a write threshold, per request, and on Drop.
        if self.unpersisted_writes.fetch_add(1, Ordering::AcqRel) + 1 >= REGISTRY_PERSIST_BATCH {
            return self.flush();
        }
        Ok(())
    }

    /// Persists the current snapshot if there are unpersisted writes.
    pub fn flush(&self) -> Result<(), String> {
        let had = self.unpersisted_writes.swap(0, Ordering::AcqRel);
        if had == 0 {
            return Ok(());
        }
        if let Err(error) = self.persist_latest_snapshot() {
            // Restore the dirty count so a later flush retries.
            self.unpersisted_writes.fetch_add(had, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    pub fn write_metadata(
        &self,
        package_name: &str,
        metadata: RegistryPackageMetadata,
        updated_at: u64,
    ) -> Result<(), String> {
        self.write_entry(
            package_name,
            RegistryPackageMetadataEntry {
                metadata: Some(metadata),
                updated_at,
                retry_after: None,
                error: None,
                not_found: false,
            },
        )
    }

    fn persist_latest_snapshot(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let Ok(_persist_guard) = self.persist_lock.lock() else {
            return Err("registry cache persist lock poisoned".to_owned());
        };
        let Ok(snapshot) = self.entries.lock().map(|entries| entries.clone()) else {
            return Err("registry cache lock poisoned".to_owned());
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
        // Persist atomically: a direct `fs::write` to the live path can truncate the
        // cache if the process crashes mid-write. Write the full last-writer-wins
        // snapshot to a temp file, then rename it over the target.
        let temp_path = self.path.with_extension("json.tmp");
        fs::write(&temp_path, bytes).map_err(|error| error.to_string())?;
        fs::rename(&temp_path, &self.path).map_err(|error| error.to_string())
    }
}

impl Drop for RegistryMetadataCache {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

pub fn cache_key(package_name: &str) -> String {
    package_name.to_owned()
}

fn load_entries(path: &Path) -> HashMap<String, RegistryPackageMetadataEntry> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}
