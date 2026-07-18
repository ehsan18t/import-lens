//! A memo for anything derived from an engine build, keyed by `(entry, runtime)`.
//!
//! An engine build is the most expensive thing the daemon does, and two callers ask
//! it questions whose answers do not depend on the request that triggered them:
//!
//! - the full-package comparison behind `truly_treeshakeable` (§8.4/§6.3), whose
//!   answer is the same for every named import of a package, while the import cache
//!   key is not — so N named variants of one entry paid for N of these builds;
//! - export enumeration for completion (§8.4), which was an uncached full build of
//!   the whole package graph on every popup.
//!
//! Both are memoized here. Correctness rests on the memo expiring exactly when the
//! value it holds would have gone wrong, which takes **two** independent guards:
//!
//! 1. **Read-time fingerprints.** The build's own fingerprints — the bytes it was
//!    actually measured from — are re-checked on every lookup with
//!    `check_fingerprints_strict`, the same validator the import cache uses, so even
//!    a first-party edit that preserves mtime and length is caught. A build whose
//!    graph held a module the plugin could not fingerprint as it read it is not
//!    memoized at all.
//!
//! 2. **The cache generation.** Fingerprints alone are not enough. `node_modules`
//!    manifests are deliberately not fingerprinted — an installed manifest cannot
//!    change without an install, and an install bumps the generation, which is the
//!    backstop the import cache leans on. Without the generation, `pnpm install`
//!    could repoint a dependency's `exports` at a different file while leaving its
//!    sources byte-identical, and every fingerprint would still hash clean over a
//!    value measured against the *old* resolution. It is also what makes these memos
//!    obey `invalidate_package` / `invalidate_all` — the user's "clear the cache"
//!    escape hatch — without either having to know they exist.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, atomic::AtomicU64, atomic::Ordering},
};

use crate::cache::key::{
    FileFingerprint, Freshness, check_fingerprints_strict, fingerprints_are_reusable,
};
use crate::ipc::protocol::ImportRuntime;

/// One entry per (entry file, runtime). A workspace touches few package entries per
/// session, and a stale entry is dropped on its next lookup, so this only needs to
/// stop unbounded growth over a long-lived daemon.
const MAX_ENTRIES: usize = 256;

type Key = (PathBuf, ImportRuntime);

#[derive(Debug, Clone)]
struct Entry<V> {
    value: V,
    fingerprints: Vec<FileFingerprint>,
    /// The cache generation the value was measured under.
    generation: u64,
    /// Identifies this exact stored value, so a lookup that found it stale can drop
    /// *it* rather than whatever a concurrent store may have put there since.
    stamp: u64,
    used_at: u64,
}

pub(crate) struct BuildMemo<V> {
    entries: Mutex<HashMap<Key, Entry<V>>>,
    tick: AtomicU64,
}

impl<V: Clone> BuildMemo<V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            tick: AtomicU64::new(0),
        }
    }

    fn tick(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Entry<V>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The memoized value, if one was stored for this entry under the current cache
    /// generation and every file it was measured from is still current.
    pub(crate) fn get(&self, entry_path: &Path, runtime: ImportRuntime) -> Option<V> {
        let key = (entry_path.to_path_buf(), runtime);
        let generation = crate::cache::memory::cache_generation();

        let (value, fingerprints, stamp) = {
            let mut entries = self.lock();
            let entry = entries.get(&key)?;
            if entry.generation != generation {
                entries.remove(&key);
                return None;
            }
            (entry.value.clone(), entry.fingerprints.clone(), entry.stamp)
        };

        // Never hold the lock across the freshness check: it stats, and may read and
        // hash, every module in the package graph.
        match check_fingerprints_strict(&fingerprints) {
            Freshness::Fresh => {}
            // `Unknown` is a transient stat/read failure — a file locked by an antivirus
            // scan, an offline mapped drive. The cache contract (see `cache::key`) is to
            // KEEP such an entry rather than evict it; we simply decline to serve it and
            // recompute. Evicting would throw away a still-good value and force a full
            // build for as long as the condition lasted.
            Freshness::Unknown => return None,
            Freshness::Stale | Freshness::Gone => {
                let mut entries = self.lock();
                // Drop the value we actually found stale, not whatever is there now: a
                // concurrent caller may already have rebuilt and stored one measured from
                // the current bytes, and removing that would just buy another full build.
                if entries.get(&key).is_some_and(|entry| entry.stamp == stamp) {
                    entries.remove(&key);
                }
                return None;
            }
        }

        if let Some(entry) = self.lock().get_mut(&key) {
            entry.used_at = self.tick.fetch_add(1, Ordering::Relaxed);
        }
        Some(value)
    }

    /// Store a value against the fingerprints of the exact bytes it was measured from.
    /// Storing nothing is always safe — the caller just rebuilds.
    ///
    /// `generation` must be the cache generation observed *before* the build ran, not
    /// after — the same discipline `analyze_and_cache` uses. An invalidation that lands
    /// while the build is in flight must not be stamped onto a value measured from the
    /// bytes it invalidated.
    pub(crate) fn insert(
        &self,
        entry_path: &Path,
        runtime: ImportRuntime,
        value: V,
        fingerprints: Vec<FileFingerprint>,
        generation: u64,
    ) {
        if fingerprints.is_empty() || !fingerprints_are_reusable(&fingerprints) {
            return;
        }

        let key = (entry_path.to_path_buf(), runtime);
        let stamp = self.tick();
        let used_at = self.tick();
        let mut entries = self.lock();

        // Only shed a victim when this insert actually grows the map. Re-storing a key
        // that is already present would otherwise evict a live entry for nothing, and at
        // a steady MAX_ENTRIES every refresh would ratchet the map down by one.
        if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
            let coldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.used_at)
                .map(|(key, _)| key.clone());
            if let Some(coldest) = coldest {
                entries.remove(&coldest);
            }
        }

        entries.insert(
            key,
            Entry {
                value,
                fingerprints,
                generation,
                stamp,
                used_at,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreusable_build_observation_never_enters_the_memo() {
        let memo = BuildMemo::<u64>::new();
        let entry = Path::new("/pkg/index.js");
        let fingerprints = vec![crate::cache::key::unverifiable_file_fingerprint(
            "/pkg/unreadable.woff2",
        )];

        memo.insert(
            entry,
            ImportRuntime::Client,
            42,
            fingerprints,
            crate::cache::memory::cache_generation(),
        );

        assert!(
            memo.lock().is_empty(),
            "a memo must not retain an observation it can never safely serve"
        );
    }
}
