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
//! A third answer is a failure: a file's combined File Cost build that failed for a
//! reason its inputs' bytes decide (`file_size_cache`), keyed by the document rather
//! than an entry. Its total is a floor and is never cached, but rebuilding it only
//! reproduces the same failure until those bytes change.
//!
//! All three are memoized here. Correctness rests on the memo expiring exactly when the
//! value it holds would have gone wrong, which takes **two** independent guards:
//!
//! 1. **Read-time fingerprints.** The build's own fingerprints (the bytes it was
//!    actually measured from) are checked with `check_fingerprints_strict`, the same
//!    validator the import cache uses. First-party inputs are hash-verified on every
//!    lookup, so even an edit that preserves mtime and length is caught. Installed
//!    (`node_modules`) inputs are re-checked once per `REVERIFY_TTL` at a given
//!    generation, exactly as the import cache's fast path trusts them: they change only
//!    through an install, which bumps the generation, and the window bounds an install
//!    no watcher reported. A build whose graph held a module the plugin could not
//!    fingerprint as it read it is not memoized at all.
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
    time::Instant,
};

use crate::cache::key::{
    FileFingerprint, Freshness, check_fingerprints_strict, fingerprint_is_installed,
    fingerprints_are_reusable,
};
use crate::cache::memory::REVERIFY_TTL;
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
    /// The subset of `fingerprints` outside `node_modules`, re-verified on every lookup.
    first_party: Vec<FileFingerprint>,
    /// When every fingerprint was last confirmed current. `None` until the first lookup.
    verified_at: Option<Instant>,
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

        let (value, fingerprints, stamp, verified_recently) = {
            let mut entries = self.lock();
            let entry = entries.get(&key)?;
            if entry.generation != generation {
                entries.remove(&key);
                return None;
            }
            // Within the window, at the generation it was measured under, an installed input
            // can only have changed with no invalidation event, which the window itself bounds
            // (the import cache's fast path). First-party inputs change with no event at all,
            // so they are re-verified on every lookup regardless (D3).
            let verified_recently = entry
                .verified_at
                .is_some_and(|at| at.elapsed() < REVERIFY_TTL);
            let fingerprints = if verified_recently {
                entry.first_party.clone()
            } else {
                entry.fingerprints.clone()
            };
            (
                entry.value.clone(),
                fingerprints,
                entry.stamp,
                verified_recently,
            )
        };

        // Anchored before the check, so no input goes unobserved for longer than the window.
        let checked_at = Instant::now();
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
            if !verified_recently && entry.stamp == stamp {
                entry.verified_at = Some(checked_at);
            }
        }
        Some(value)
    }

    /// Drop whatever is stored for this entry, so a value the caller knows no longer applies is
    /// not re-verified on every later lookup.
    pub(crate) fn remove(&self, entry_path: &Path, runtime: ImportRuntime) {
        self.lock().remove(&(entry_path.to_path_buf(), runtime));
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
        let first_party = fingerprints
            .iter()
            .filter(|fingerprint| !fingerprint_is_installed(fingerprint))
            .cloned()
            .collect();
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
                first_party,
                verified_at: None,
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

    /// A lookup inside the window does not re-stat installed inputs, which is what makes a
    /// completion popup cheap; a lookup after it does, and a first-party input is hash-verified on
    /// every lookup whatever the window says.
    #[test]
    fn installed_inputs_are_rechecked_once_per_window_and_first_party_ones_every_time() {
        let _generation = crate::cache::memory::hold_cache_generation_steady();
        let directory = std::env::temp_dir().join(format!(
            "import-lens-build-memo-window-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let installed = directory.join("node_modules").join("pkg").join("index.js");
        let first_party = directory.join("src").join("index.js");
        for (path, body) in [
            (&installed, "export const a = 1;\n"),
            (&first_party, "export const b = 1;\n"),
        ] {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, body).expect("write");
        }
        let fingerprint = |path: &Path| {
            vec![crate::cache::key::file_fingerprint_reading_hash(path).expect("fingerprint")]
        };
        let generation = crate::cache::memory::cache_generation();
        let memo = BuildMemo::<u64>::new();
        let runtime = ImportRuntime::Client;

        memo.insert(&installed, runtime, 1, fingerprint(&installed), generation);
        assert_eq!(
            memo.get(&installed, runtime),
            Some(1),
            "first lookup verifies all"
        );
        std::fs::write(&installed, "export const a = 'changed';\n").expect("rewrite");
        assert_eq!(
            memo.get(&installed, runtime),
            Some(1),
            "inside the window an installed input is not re-stat'd"
        );
        if let Some(expired) = Instant::now().checked_sub(REVERIFY_TTL + REVERIFY_TTL) {
            if let Some(entry) = memo.lock().get_mut(&(installed.clone(), runtime)) {
                entry.verified_at = Some(expired);
            }
            assert_eq!(
                memo.get(&installed, runtime),
                None,
                "after the window the installed input is checked again"
            );
        }

        memo.insert(
            &first_party,
            runtime,
            2,
            fingerprint(&first_party),
            generation,
        );
        assert_eq!(memo.get(&first_party, runtime), Some(2));
        std::fs::write(&first_party, "export const b = 2;\n").expect("same-length rewrite");
        assert_eq!(
            memo.get(&first_party, runtime),
            None,
            "a first-party edit expires the memo on the very next lookup"
        );

        std::fs::remove_dir_all(directory).expect("cleanup");
    }
}
