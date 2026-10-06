use super::{
    constants::{REGISTRY_CACHE_FILE_NAME, REGISTRY_RETENTION_MS},
    types::{RegistryPackageMetadata, RegistryPackageMetadataEntry},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

// Persist the full snapshot at most every N writes, so refreshing M packages does not rewrite
// the whole file M times. A trailing flush (per request / on Drop) persists the remainder.
const REGISTRY_PERSIST_BATCH: usize = 16;

/// On-disk schema version for the registry metadata file. Bump it on any format change:
/// `load_snapshot` wipes a file written under another version instead of misparsing it. Independent
/// of the bundle-cache version (§11).
const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Versioned envelope wrapping the persisted entry map, so the loader can detect a wrong
/// `schema_version` (or a bare-map file, which fails to parse) and wipe instead of misreading.
#[derive(Default, Serialize, Deserialize)]
struct RegistrySnapshot {
    schema_version: u32,
    entries: HashMap<String, RegistryPackageMetadataEntry>,
    /// Unix millis of the last `clear()` by any daemon sharing the file. A daemon that still holds
    /// entries from before it drops them instead of writing them back.
    #[serde(default)]
    cleared_at: u64,
}

/// Borrowing twin of `RegistrySnapshot`, so serializing (and measuring) never clones the map. Its
/// field order must match `RegistrySnapshot`, so it writes the same bytes.
#[derive(Serialize)]
struct RegistrySnapshotRef<'a> {
    schema_version: u32,
    entries: &'a HashMap<String, RegistryPackageMetadataEntry>,
    cleared_at: u64,
}

#[derive(Debug)]
pub struct RegistryMetadataCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, RegistryPackageMetadataEntry>>,
    /// The newest `cleared_at` this process has applied to `entries`.
    cleared_at: AtomicU64,
    persist_lock: Mutex<()>,
    unpersisted_writes: AtomicUsize,
}

impl RegistryMetadataCache {
    pub fn new(storage_path: PathBuf) -> Self {
        let path = storage_path.join(REGISTRY_CACHE_FILE_NAME);
        let snapshot = load_snapshot(&path);
        Self {
            path,
            entries: Mutex::new(snapshot.entries),
            cleared_at: AtomicU64::new(snapshot.cleared_at),
            persist_lock: Mutex::new(()),
            unpersisted_writes: AtomicUsize::new(0),
        }
    }

    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Mutex::new(HashMap::new()),
            cleared_at: AtomicU64::new(0),
            persist_lock: Mutex::new(()),
            unpersisted_writes: AtomicUsize::new(0),
        }
    }

    /// Apply a clear another daemon recorded in the shared file: drop every entry this process
    /// still holds from before it. Called under the entries lock, before a merge with disk.
    fn adopt_clear(
        &self,
        entries: &mut HashMap<String, RegistryPackageMetadataEntry>,
        cleared_at: u64,
    ) {
        if cleared_at > self.cleared_at.load(Ordering::Acquire) {
            entries.retain(|_, entry| entry.updated_at > cleared_at);
            self.cleared_at.store(cleared_at, Ordering::Release);
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

    pub fn get_many<I, S>(&self, package_names: I) -> Vec<Option<RegistryPackageMetadataEntry>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let keys: Vec<_> = package_names
            .into_iter()
            .map(|package_name| cache_key(package_name.as_ref()))
            .collect();
        let Ok(entries) = self.entries.lock() else {
            return vec![None; keys.len()];
        };
        keys.iter().map(|key| entries.get(key).cloned()).collect()
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
        // The in-memory map is the source of truth, so the full-file persist is deferred.
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
        if let Err(error) = self.persist_snapshot() {
            // Restore the dirty count so a later flush retries.
            self.unpersisted_writes.fetch_add(had, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    /// Serialized size in bytes of the in-memory snapshot (the versioned envelope), for cache
    /// status. One O(entries) serialization, never on a write hot path. A poisoned lock gives 0.
    pub fn serialized_size_bytes(&self) -> u64 {
        self.entries
            .lock()
            .map(|entries| snapshot_bytes(&entries))
            .unwrap_or(0)
    }

    /// Empties the store and writes an authoritative empty snapshot that bypasses the
    /// persist-time union, so the cleared entries do not resurrect from disk on the next save.
    pub fn clear(&self) -> Result<(), String> {
        // No backing file (disabled / `empty()` cache): just empty the in-memory map.
        if self.path.as_os_str().is_empty() {
            if let Ok(mut entries) = self.entries.lock() {
                entries.clear();
            }
            self.unpersisted_writes.store(0, Ordering::Release);
            return Ok(());
        }
        // Take `persist_lock` FIRST, matching persist_snapshot's lock order
        // (persist_lock -> entries), so the two can never deadlock.
        let Ok(_persist_guard) = self.persist_lock.lock() else {
            return Err("registry cache persist lock poisoned".to_owned());
        };
        // Clear, capture the snapshot, and reset the pending-write count under one entries-lock
        // hold. A concurrent `write_entry` then lands either before the clear (dropped with it)
        // or after (its own `fetch_add` re-counts it). Resetting after the write below instead
        // would clobber a post-clear write's dirty flag.
        let snapshot = {
            let Ok(mut entries) = self.entries.lock() else {
                return Err("registry cache lock poisoned".to_owned());
            };
            entries.clear();
            self.cleared_at
                .fetch_max(crate::time::unix_millis_now(), Ordering::AcqRel);
            self.unpersisted_writes.store(0, Ordering::Release);
            entries.clone()
        };
        // Written verbatim, never merged with the file, so the cleared entries do not come back
        // off disk on the next save; `cleared_at` makes a sibling daemon drop its copies too.
        self.write_snapshot(&snapshot)
    }

    /// Retention prune for the user-triggered orphan purge: drops entries older than
    /// `retention_ms`, written authoritatively so the deletions stick. Returns the number removed.
    /// The maintenance pass uses [`Self::run_maintenance`], which adds the size cap.
    pub fn purge_expired(&self, now_ms: u64, retention_ms: u64) -> usize {
        self.compact_authoritatively(now_ms, retention_ms, None)
    }

    /// Registry-store maintenance: the retention prune, then a byte-budget cap (evict
    /// oldest-`updated_at` entries until the snapshot fits `max_bytes`), then one authoritative
    /// write. Runs on the per-open maintenance pass (decision-log D3), never on the write hot path,
    /// where measuring the size is too costly. Returns the total entries removed.
    pub fn run_maintenance(&self, now_ms: u64, max_bytes: u64) -> usize {
        self.compact_authoritatively(now_ms, REGISTRY_RETENTION_MS, Some(max_bytes))
    }

    /// Shared body for the orphan purge and the maintenance pass. Merges the on-disk view in first
    /// (newest `updated_at` per key), so a sibling process's writes survive and entries only a
    /// closed window held are still pruned; then prunes, optionally evicts down to `max_bytes`,
    /// and writes authoritatively. A union write would merge the dropped entries straight back.
    fn compact_authoritatively(
        &self,
        now_ms: u64,
        retention_ms: u64,
        max_bytes: Option<u64>,
    ) -> usize {
        // No backing file (disabled / `empty()` cache): prune in memory only.
        if self.path.as_os_str().is_empty() {
            return match self.entries.lock() {
                Ok(mut entries) => {
                    let mut removed = prune_expired_entries(&mut entries, now_ms, retention_ms);
                    if let Some(max_bytes) = max_bytes {
                        removed += evict_oldest_over_budget(&mut entries, max_bytes);
                    }
                    self.unpersisted_writes.store(0, Ordering::Release);
                    removed
                }
                Err(_) => 0,
            };
        }
        // Take `persist_lock` FIRST, matching persist_snapshot's lock order
        // (persist_lock -> entries), so the two can never deadlock.
        let Ok(_persist_guard) = self.persist_lock.lock() else {
            return 0;
        };
        // Merge, prune, evict, reset the pending-write count, and capture the snapshot under one
        // entries-lock hold, for the same reason as in `clear()`.
        let (removed, snapshot) = {
            let Ok(mut entries) = self.entries.lock() else {
                return 0;
            };
            let on_disk = load_snapshot(&self.path);
            self.adopt_clear(&mut entries, on_disk.cleared_at);
            merge_newest(&mut entries, on_disk.entries);
            let mut removed = prune_expired_entries(&mut entries, now_ms, retention_ms);
            if let Some(max_bytes) = max_bytes {
                removed += evict_oldest_over_budget(&mut entries, max_bytes);
            }
            self.unpersisted_writes.store(0, Ordering::Release);
            (removed, entries.clone())
        };
        // Written verbatim, never unioned with the file, so the deletions stick.
        let _ = self.write_snapshot(&snapshot);
        removed
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

    /// Writes the current snapshot unioned with the on-disk view. The file is shared by every
    /// workspace's daemon, so another process may have persisted entries since this one loaded:
    /// the newest `updated_at` per package wins instead of this write clobbering theirs. A clear
    /// another daemon recorded is applied first, so the entries it removed are not written back.
    /// A tiny cross-process read->rename window remains.
    ///
    /// The authoritative writes (`clear`, the maintenance compaction) do not come through here:
    /// a union would merge the entries they just dropped straight back in.
    fn persist_snapshot(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let Ok(_persist_guard) = self.persist_lock.lock() else {
            return Err("registry cache persist lock poisoned".to_owned());
        };
        let on_disk = load_snapshot(&self.path);
        let Ok(mut snapshot) = self.entries.lock().map(|mut entries| {
            self.adopt_clear(&mut entries, on_disk.cleared_at);
            entries.clone()
        }) else {
            return Err("registry cache lock poisoned".to_owned());
        };
        merge_newest(&mut snapshot, on_disk.entries);
        self.write_snapshot(&snapshot)
    }

    /// Serializes `snapshot` into the versioned envelope and writes it atomically (temp file +
    /// rename). Takes no locks; callers hold `persist_lock`.
    fn write_snapshot(
        &self,
        snapshot: &HashMap<String, RegistryPackageMetadataEntry>,
    ) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec(&RegistrySnapshotRef {
            schema_version: REGISTRY_SCHEMA_VERSION,
            entries: snapshot,
            cleared_at: self.cleared_at.load(Ordering::Acquire),
        })
        .map_err(|error| error.to_string())?;
        // Temp file + rename, so a crash mid-write cannot truncate the live file. The temp name is
        // per process: the file is shared global storage, and a fixed temp path would let two
        // windows interleave writes and rename corrupt JSON into place.
        let temp_path = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
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

fn load_snapshot(path: &Path) -> RegistrySnapshot {
    let Ok(contents) = fs::read_to_string(path) else {
        return RegistrySnapshot::default();
    };
    // A parse failure or an unrecognized `schema_version` yields an empty map rather than a
    // misparse (§11). Scoped to the registry file; it never touches the bundle shards.
    match serde_json::from_str::<RegistrySnapshot>(&contents) {
        Ok(snapshot) if snapshot.schema_version == REGISTRY_SCHEMA_VERSION => snapshot,
        _ => RegistrySnapshot::default(),
    }
}

/// Merge `on_disk` into `entries`, keeping the newest `updated_at` per package.
fn merge_newest(
    entries: &mut HashMap<String, RegistryPackageMetadataEntry>,
    on_disk: HashMap<String, RegistryPackageMetadataEntry>,
) {
    for (key, on_disk) in on_disk {
        let keep_ours = entries
            .get(&key)
            .is_some_and(|ours| ours.updated_at >= on_disk.updated_at);
        if !keep_ours {
            entries.insert(key, on_disk);
        }
    }
}

fn prune_expired_entries(
    entries: &mut HashMap<String, RegistryPackageMetadataEntry>,
    now_ms: u64,
    retention_ms: u64,
) -> usize {
    let before = entries.len();
    // A failure for a package never fetched successfully has no `updated_at` to age by, so its
    // open retry window is what keeps it.
    entries.retain(|_, entry| {
        entry.retry_after.is_some_and(|retry_at| retry_at > now_ms)
            || now_ms.saturating_sub(entry.updated_at) <= retention_ms
    });
    before - entries.len()
}

/// Serialized length of the versioned envelope for `entries`, measured the way `write_snapshot`
/// writes it. `cleared_at` is measured at its widest, so the estimate never falls short.
fn snapshot_bytes(entries: &HashMap<String, RegistryPackageMetadataEntry>) -> u64 {
    serde_json::to_vec(&RegistrySnapshotRef {
        schema_version: REGISTRY_SCHEMA_VERSION,
        entries,
        cleared_at: u64::MAX,
    })
    .map(|bytes| bytes.len() as u64)
    .unwrap_or(0)
}

/// Approximate serialized footprint of one `"key":value` pair in the entries object (value JSON,
/// quoted key, colon, comma), so `evict_oldest_over_budget` need not re-serialize per removal.
fn entry_footprint(key: &str, entry: &RegistryPackageMetadataEntry) -> u64 {
    let value_len = serde_json::to_vec(entry)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    // key + two quotes + ':' + ',' framing.
    (key.len() + value_len + 4) as u64
}

/// Evicts entries by ascending `updated_at` (oldest first; the key breaks ties
/// so eviction is deterministic) until the serialized snapshot fits within
/// `max_bytes`. Returns the number evicted.
///
/// Two phases keep this O(n): subtract each victim's [`entry_footprint`] from a running total,
/// then re-measure exactly and drop a few more if the estimate (which drifts ~1 byte per entry
/// from comma framing) stopped just over budget.
fn evict_oldest_over_budget(
    entries: &mut HashMap<String, RegistryPackageMetadataEntry>,
    max_bytes: u64,
) -> usize {
    // A zero budget means "no size cap", matching the main byte budget, never "evict everything"
    // (decision-log D10).
    if max_bytes == 0 {
        return 0;
    }
    if snapshot_bytes(entries) <= max_bytes {
        return 0;
    }
    let mut ordered: Vec<String> = entries.keys().cloned().collect();
    ordered.sort_by(|a, b| {
        entries[a]
            .updated_at
            .cmp(&entries[b].updated_at)
            .then_with(|| a.cmp(b))
    });

    let mut evicted = 0;
    let mut estimated = snapshot_bytes(entries);
    let mut ordered = ordered.into_iter();
    for key in ordered.by_ref() {
        if estimated <= max_bytes {
            break;
        }
        estimated = estimated.saturating_sub(entry_footprint(&key, &entries[&key]));
        entries.remove(&key);
        evicted += 1;
    }
    // Exact reconciliation against the real envelope size.
    while snapshot_bytes(entries) > max_bytes {
        let Some(key) = ordered.next() else { break };
        entries.remove(&key);
        evicted += 1;
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::constants::REGISTRY_RETENTION_MS;

    fn entry(updated_at: u64) -> RegistryPackageMetadataEntry {
        RegistryPackageMetadataEntry {
            metadata: None,
            updated_at,
            retry_after: None,
            error: None,
            not_found: false,
        }
    }

    #[test]
    fn prune_expired_entries_drops_only_stale_rows() {
        let now = 100 * REGISTRY_RETENTION_MS;
        let mut entries = HashMap::new();
        entries.insert("fresh".to_owned(), entry(now));
        entries.insert("edge".to_owned(), entry(now - REGISTRY_RETENTION_MS));
        entries.insert("stale".to_owned(), entry(now - REGISTRY_RETENTION_MS - 1));

        let removed = prune_expired_entries(&mut entries, now, REGISTRY_RETENTION_MS);

        assert_eq!(removed, 1);
        assert!(entries.contains_key("fresh"));
        assert!(entries.contains_key("edge"));
        assert!(!entries.contains_key("stale"));
    }

    #[test]
    fn zero_budget_disables_size_eviction_instead_of_wiping_everything() {
        // A hand-edited `registryCacheMaxSizeMB: 0` means "no size cap", not "evict every hint".
        let mut entries = HashMap::new();
        entries.insert("react".to_owned(), entry(1_000));
        entries.insert("lodash".to_owned(), entry(2_000));

        let removed = evict_oldest_over_budget(&mut entries, 0);

        assert_eq!(removed, 0, "a zero budget must not evict anything");
        assert_eq!(
            entries.len(),
            2,
            "the whole hint store survives a zero budget"
        );
    }

    #[test]
    fn clear_persists_empty_snapshot_authoritatively() {
        let dir = std::env::temp_dir().join(format!(
            "il-registry-clear-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cache = RegistryMetadataCache::new(dir.clone());
        cache
            .write_entry("react", entry(1_000))
            .expect("seed a metadata entry");
        cache.flush().expect("persist the seeded entry");
        assert!(cache.get("react").is_some());

        // clear() empties the in-memory store and writes an authoritative empty snapshot.
        cache
            .clear()
            .expect("clear should persist the empty snapshot");
        assert!(
            cache.get("react").is_none(),
            "clear empties the in-memory store"
        );

        // A fresh load sees the cleared state, not entries resurrected by a union write.
        let reloaded = RegistryMetadataCache::new(dir.clone());
        assert!(
            reloaded.get("react").is_none(),
            "the cleared state is persisted authoritatively"
        );

        drop(cache);
        drop(reloaded);
        std::fs::remove_dir_all(&dir).ok();
    }
}
