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
        atomic::{AtomicUsize, Ordering},
    },
};

// Persist the full snapshot at most every N writes rather than on every write,
// so refreshing M packages does not rewrite the whole file M times (O(M^2)
// bytes). A trailing flush (per request / on Drop) persists the remainder.
const REGISTRY_PERSIST_BATCH: usize = 16;

/// On-disk schema version for the registry metadata file. Bumping this (any
/// format change to the persisted entries) makes `load_entries` wipe a file
/// written under a different version instead of misparsing it. Scoped to the
/// registry file only: a bundle-cache bump never touches this and vice-versa
/// (§11).
const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Versioned envelope wrapping the persisted entry map. Storing the bare
/// `HashMap` gave `load_entries` no way to tell a schema change from valid data;
/// wrapping it lets the loader detect a wrong `schema_version` (or a
/// pre-envelope bare-map file, which simply fails to parse as this struct) and
/// wipe rather than misinterpret stale bytes.
#[derive(Serialize, Deserialize)]
struct RegistrySnapshot {
    schema_version: u32,
    entries: HashMap<String, RegistryPackageMetadataEntry>,
}

/// Borrowing twin of `RegistrySnapshot` used only to MEASURE the serialized
/// snapshot size (the size cap) without cloning the whole map on every check.
/// Its field order matches `RegistrySnapshot`, so the measured length equals the
/// bytes `persist_snapshot` will actually write.
#[derive(Serialize)]
struct RegistrySnapshotRef<'a> {
    schema_version: u32,
    entries: &'a HashMap<String, RegistryPackageMetadataEntry>,
}

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
        if let Err(error) = self.persist_snapshot(None, true) {
            // Restore the dirty count so a later flush retries.
            self.unpersisted_writes.fetch_add(had, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    /// Serialized size in bytes of the current in-memory snapshot, measured
    /// exactly as [`Self::persist_snapshot`] writes it (the versioned envelope).
    /// One O(entries) serialization for cache-status observability (§8/X-24),
    /// reusing the same [`snapshot_bytes`] measurement the size cap uses — never
    /// on a write hot path. A poisoned lock degrades to 0.
    pub fn serialized_size_bytes(&self) -> u64 {
        self.entries
            .lock()
            .map(|entries| snapshot_bytes(&entries))
            .unwrap_or(0)
    }

    /// Empties the store and writes an authoritative empty snapshot that bypasses
    /// the persist-time union, so the cleared entries do not resurrect from disk
    /// on the next save (X-14). A normal `flush` keeps the union for cross-process
    /// safety; only this authoritative path may deliberately shrink the shared
    /// file to nothing.
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
        // Empty the map, CAPTURE the authoritative (empty) snapshot, AND reset the
        // pending-write count under ONE entries-lock hold. This closes the D-a race: a
        // concurrent write_entry can no longer land between the clear and the snapshot
        // capture. It is serialized either before the clear (its entry is dropped, and
        // resetting the count here is correct — nothing of it remains) or after it (a
        // fresh post-clear write whose own fetch_add re-counts it, so a later flush
        // persists it). Doing the reset WITH the clear — not after the write below — is
        // what stops it from clobbering a post-clear write's dirty flag.
        let snapshot = {
            let Ok(mut entries) = self.entries.lock() else {
                return Err("registry cache lock poisoned".to_owned());
            };
            entries.clear();
            self.unpersisted_writes.store(0, Ordering::Release);
            entries.clone()
        };
        // union = false: the captured empty snapshot becomes the file verbatim, so the
        // cleared entries are not merged back off disk on the next save (X-14).
        self.write_snapshot(&snapshot)
    }

    /// Retention prune: drops entries whose `updated_at` is older than
    /// `retention_ms`, written AUTHORITATIVELY so the deletions stick. Invoked by
    /// the user-triggered orphan purge; the automatic startup/periodic pass goes
    /// through [`run_maintenance`], which layers the size cap on top. Returns the
    /// number of entries removed.
    pub fn purge_expired(&self, now_ms: u64, retention_ms: u64) -> usize {
        self.compact_authoritatively(now_ms, retention_ms, None)
    }

    /// Periodic registry-store maintenance (D3 + D4 / §6.1): the 30-day retention
    /// prune followed by a byte-budget size cap (evict oldest-`updated_at`
    /// entries until the serialized snapshot fits `max_bytes`), then ONE
    /// authoritative write. Runs on the maintenance pass — daemon startup and the
    /// periodic tick — never on the write hot path, where serializing to measure
    /// the size would be too costly. Returns the total entries removed (retention
    /// + eviction).
    pub fn run_maintenance(&self, now_ms: u64, max_bytes: u64) -> usize {
        self.compact_authoritatively(now_ms, REGISTRY_RETENTION_MS, Some(max_bytes))
    }

    /// Shared body for the orphan purge and the maintenance pass. Merges the
    /// on-disk view into memory FIRST (newest `updated_at` per key) so a sibling
    /// process's fresh writes survive this authoritative rewrite, and so entries
    /// only another (now-closed) window ever held are still subject to retention
    /// and the size cap instead of lingering on disk forever; then prunes
    /// past-retention entries, optionally evicts oldest entries down to
    /// `max_bytes`, and writes the result with `union = false`.
    ///
    /// The authoritative write is what makes both the retention and the eviction
    /// deletions stick: a union write would re-read disk and merge every
    /// just-dropped entry straight back in, resurrecting it.
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
        // Merge the on-disk view in, prune, evict, reset the pending-write count, AND
        // capture the authoritative snapshot under ONE entries-lock hold — the same
        // discipline as `clear()`. Doing the `store(0)` HERE (not after the write
        // below) is what stops a concurrent `write_entry` landing between the snapshot
        // capture and the reset from having its dirty flag clobbered: the D-a race,
        // closed for `clear()` in F-b and now for the maintenance path too.
        let (removed, snapshot) = {
            let Ok(mut entries) = self.entries.lock() else {
                return 0;
            };
            // Merge before pruning so the prune/evict operate on the union of every
            // process's writes and the authoritative write below cannot silently
            // clobber a sibling window's fresh disjoint entries.
            for (key, on_disk) in load_entries(&self.path) {
                let keep_ours = entries
                    .get(&key)
                    .is_some_and(|ours| ours.updated_at >= on_disk.updated_at);
                if !keep_ours {
                    entries.insert(key, on_disk);
                }
            }
            let mut removed = prune_expired_entries(&mut entries, now_ms, retention_ms);
            if let Some(max_bytes) = max_bytes {
                removed += evict_oldest_over_budget(&mut entries, max_bytes);
            }
            self.unpersisted_writes.store(0, Ordering::Release);
            (removed, entries.clone())
        };
        // union = false: the captured, merged-then-pruned-then-evicted snapshot becomes
        // the file verbatim, so the deletions are not resurrected off disk on the next save.
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

    /// Writes the current snapshot. `prune_older_than = Some((now, retention))`
    /// drops entries past the retention window from the snapshot before writing,
    /// so the orphan purge's deletions stick; `None` keeps every entry (the
    /// default flush path — automatic pruning would break tests that persist
    /// entries with synthetic timestamps).
    ///
    /// `union == true` merges the on-disk view in before writing — the
    /// cross-process safety for normal flushes. `union == false` writes exactly
    /// the in-memory snapshot (after any prune), AUTHORITATIVELY: it does not
    /// merge the on-disk entries back in, which is what lets a `clear()` (or a
    /// retention deletion) actually shrink the shared file instead of being
    /// resurrected from disk on the next save.
    fn persist_snapshot(
        &self,
        prune_older_than: Option<(u64, u64)>,
        union: bool,
    ) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let Ok(_persist_guard) = self.persist_lock.lock() else {
            return Err("registry cache persist lock poisoned".to_owned());
        };
        let Ok(mut snapshot) = self.entries.lock().map(|entries| entries.clone()) else {
            return Err("registry cache lock poisoned".to_owned());
        };
        // The registry cache is shared across every workspace's daemon via global
        // storage. Another process may have persisted entries since we loaded, so
        // union the on-disk view in (keeping the newest `updated_at` per package)
        // before this full-snapshot write, instead of clobbering their entries.
        // A tiny cross-process read->rename race window remains, but this turns
        // "clobber everything another process wrote" into "clobber only what it
        // wrote in the few ms between our read and rename".
        //
        // An authoritative write (`union == false`) intentionally skips this: the
        // caller wants the in-memory snapshot to become the file verbatim.
        if union {
            for (key, on_disk) in load_entries(&self.path) {
                let keep_ours = snapshot
                    .get(&key)
                    .is_some_and(|ours| ours.updated_at >= on_disk.updated_at);
                if !keep_ours {
                    snapshot.insert(key, on_disk);
                }
            }
        }
        if let Some((now_ms, retention_ms)) = prune_older_than {
            prune_expired_entries(&mut snapshot, now_ms, retention_ms);
        }
        self.write_snapshot(&snapshot)
    }

    /// Serializes `snapshot` into the versioned envelope and writes it atomically
    /// (temp file + rename). Touches NO locks, so it is shared by `persist_snapshot`
    /// (after its clone/union/prune, under `persist_lock`) and `clear` (which captures
    /// its own empty snapshot under the entries lock). Callers hold `persist_lock`.
    fn write_snapshot(
        &self,
        snapshot: &HashMap<String, RegistryPackageMetadataEntry>,
    ) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        // Measured byte-identically by `snapshot_bytes`/the size cap: `RegistrySnapshotRef`
        // mirrors `RegistrySnapshot`'s field order, so the borrow serializes to the same
        // bytes an owned `RegistrySnapshot` would — without cloning the map.
        let bytes = serde_json::to_vec(&RegistrySnapshotRef {
            schema_version: REGISTRY_SCHEMA_VERSION,
            entries: snapshot,
        })
        .map_err(|error| error.to_string())?;
        // Persist atomically: a direct `fs::write` to the live path can truncate the
        // cache if the process crashes mid-write. Write the full last-writer-wins
        // snapshot to a temp file, then rename it over the target.
        // Per-process temp name: the cache lives in shared global storage, so a
        // fixed temp path would let two windows' writes interleave into one file
        // and rename corrupt JSON into place (which load_entries then silently
        // resets to empty). Each process writes its own complete, merged file;
        // renames are atomic and the last one wins with a superset snapshot.
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

fn load_entries(path: &Path) -> HashMap<String, RegistryPackageMetadataEntry> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    // Wipe on schema mismatch: a parse failure (e.g. a pre-envelope bare-map
    // file, or a truncated/corrupt write) or a `schema_version` this build does
    // not recognize yields an EMPTY map rather than a misparse. This is the
    // sanctioned one-time cold-cache moment (§11), scoped to the registry file —
    // it never touches the bundle shards.
    match serde_json::from_str::<RegistrySnapshot>(&contents) {
        Ok(snapshot) if snapshot.schema_version == REGISTRY_SCHEMA_VERSION => snapshot.entries,
        _ => HashMap::new(),
    }
}

fn prune_expired_entries(
    entries: &mut HashMap<String, RegistryPackageMetadataEntry>,
    now_ms: u64,
    retention_ms: u64,
) -> usize {
    let before = entries.len();
    entries.retain(|_, entry| now_ms.saturating_sub(entry.updated_at) <= retention_ms);
    before - entries.len()
}

/// Serialized length of the versioned envelope for `entries`, measured exactly
/// as `persist_snapshot` writes it, so a size-cap check matches the eventual
/// on-disk file size. Borrows the map (via `RegistrySnapshotRef`) to avoid
/// cloning it on every measurement.
fn snapshot_bytes(entries: &HashMap<String, RegistryPackageMetadataEntry>) -> u64 {
    serde_json::to_vec(&RegistrySnapshotRef {
        schema_version: REGISTRY_SCHEMA_VERSION,
        entries,
    })
    .map(|bytes| bytes.len() as u64)
    .unwrap_or(0)
}

/// Approximate serialized footprint of one `"key":value` pair inside the entries
/// object: the value's own JSON length, the quoted key, the colon, and one comma
/// separator. Lets `evict_oldest_over_budget` bulk-evict without re-serializing
/// the whole envelope per removal; the exact reconciliation there covers the
/// small JSON-framing drift.
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
/// Two phases keep this O(n) rather than O(n^2). Phase one subtracts each
/// victim's own [`entry_footprint`] from a running total instead of
/// re-serializing the full envelope per removal. Phase two re-measures exactly
/// once and drops a few more oldest entries if the per-entry estimate stopped a
/// hair over budget (the comma framing makes it drift by ~1 byte per entry).
fn evict_oldest_over_budget(
    entries: &mut HashMap<String, RegistryPackageMetadataEntry>,
    max_bytes: u64,
) -> usize {
    // A zero budget means "no size cap" (disabled), matching the main byte budget
    // (`budget.rs` returns early on `budget_bytes == 0`) — NOT "evict everything".
    // Without this, a hand-edited `registryCacheMaxSizeMB: 0` would wipe the entire
    // hint store on every maintenance pass; RB-16 made this value live end-to-end,
    // where the old hardcoded 32 MiB constant could never reach zero.
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
    // Exact reconciliation against the real envelope size: drop a few more oldest
    // entries if the per-entry estimate left us fractionally over budget.
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
        // RB-16 hazard guard: a hand-edited `registryCacheMaxSizeMB: 0` (out of the
        // package.json schema) must mean "no size cap" — NOT "evict every hint".
        // Without the `max_bytes == 0` guard, `evict_oldest_over_budget` drains the
        // whole store on every maintenance pass.
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

        // clear() empties the in-memory store AND writes an authoritative empty
        // snapshot (union-bypassing), returning the write's Result (D-a).
        cache
            .clear()
            .expect("clear should persist the empty snapshot");
        assert!(
            cache.get("react").is_none(),
            "clear empties the in-memory store"
        );

        // A fresh load from the same file sees the cleared state — the empty snapshot
        // is durable, not resurrected off disk by a union write.
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
