//! Memo for the full-package comparison build (§8.4/§6.3).
//!
//! `truly_treeshakeable` asks one question — is the named import materially
//! smaller than the whole package? — and answers it with a second complete
//! Rolldown build plus a second complete minify, of which only the minified
//! *length* is ever used. That answer does not depend on which names were
//! imported, but the import cache key does, so for N named variants of one
//! entry the daemon paid 2N full builds. This memo collapses the second build
//! to one per `(entry, runtime)`.
//!
//! Correctness rests on the memo expiring exactly when the size it stores would:
//! the full build's own read-time fingerprints are kept alongside the length and
//! re-checked on every lookup with `check_fingerprints_strict` — the same
//! validator the import cache uses, so a first-party edit that preserves mtime
//! and length is still caught. A build whose graph contained a module the plugin
//! could not fingerprint at read time is not memoized at all.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, atomic::AtomicU64, atomic::Ordering},
};

use crate::cache::key::{FileFingerprint, Freshness, check_fingerprints_strict};
use crate::ipc::protocol::ImportRuntime;

/// One entry per (entry file, runtime). A workspace touches few package entries
/// per session, and a stale entry is dropped on its next lookup, so this only
/// needs to stop unbounded growth over a long-lived daemon.
const MAX_ENTRIES: usize = 256;

#[derive(Debug, Clone)]
struct Memo {
    minified_len: u64,
    fingerprints: Vec<FileFingerprint>,
    /// The cache generation this length was measured under.
    ///
    /// Fingerprints alone are not enough. `first_party_manifests` deliberately skips
    /// anything under `node_modules`, because an installed manifest cannot change
    /// without an install — and an install bumps the generation, which is what the
    /// import cache leans on. A memo with no generation would not get that backstop:
    /// `pnpm install` could repoint a dependency's `exports` at a different file
    /// while leaving its sources byte-identical, and every fingerprint would still
    /// hash clean over a length measured against the *old* resolution.
    ///
    /// This is also what makes the memo obey `invalidate_package` / `invalidate_all`
    /// — the user's "clear the cache" escape hatch — without either having to know it
    /// exists.
    generation: u64,
    /// Identifies this exact stored value, so a lookup that found it stale can drop
    /// *it* rather than whatever a concurrent store may have put there since.
    stamp: u64,
    used_at: u64,
}

static MEMOS: LazyLock<Mutex<HashMap<(PathBuf, ImportRuntime), Memo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TICK: AtomicU64 = AtomicU64::new(0);

fn tick() -> u64 {
    TICK.fetch_add(1, Ordering::Relaxed)
}

/// The memoized full-package minified length, if one was stored for this entry
/// under the current cache generation and every file it was measured from is
/// still current. Anything else rebuilds.
pub(crate) fn lookup(entry_path: &Path, runtime: ImportRuntime) -> Option<u64> {
    let key = (entry_path.to_path_buf(), runtime);
    let generation = crate::cache::memory::cache_generation();

    let (minified_len, fingerprints, stamp) = {
        let mut memos = MEMOS
            .lock()
            .expect("full-package memo should not be poisoned");
        let memo = memos.get(&key)?;
        if memo.generation != generation {
            memos.remove(&key);
            return None;
        }
        (memo.minified_len, memo.fingerprints.clone(), memo.stamp)
    };

    // Never hold the lock across the freshness check: it stats, and may read and
    // hash, every module in the package graph.
    match check_fingerprints_strict(&fingerprints) {
        Freshness::Fresh => {}
        // `Unknown` is a transient stat/read failure — a file locked by an antivirus
        // scan, an offline mapped drive. The cache contract (see `cache::key`) is to
        // KEEP such an entry rather than evict it; we simply decline to serve it, and
        // rebuild. Evicting would throw away a still-good length and force a full
        // build for as long as the condition lasted.
        Freshness::Unknown => return None,
        Freshness::Stale | Freshness::Gone => {
            let mut memos = MEMOS
                .lock()
                .expect("full-package memo should not be poisoned");
            // Drop the value we actually found stale, not whatever is there now: a
            // concurrent lookup may already have rebuilt and stored one measured from
            // the current bytes, and removing that would just buy another full build.
            if memos.get(&key).is_some_and(|memo| memo.stamp == stamp) {
                memos.remove(&key);
            }
            return None;
        }
    }

    if let Some(memo) = MEMOS
        .lock()
        .expect("full-package memo should not be poisoned")
        .get_mut(&key)
    {
        memo.used_at = tick();
    }
    Some(minified_len)
}

/// Store a full-package length against the fingerprints of the exact bytes it was
/// measured from. Storing nothing is always safe — the caller just rebuilds.
///
/// `generation` must be the cache generation observed *before* the build ran, not
/// after — the same discipline `analyze_and_cache` uses. An invalidation that lands
/// while the build is in flight must not be stamped onto a length measured from the
/// bytes it invalidated.
pub(crate) fn store(
    entry_path: &Path,
    runtime: ImportRuntime,
    minified_len: u64,
    fingerprints: Vec<FileFingerprint>,
    generation: u64,
) {
    if fingerprints.is_empty() {
        return;
    }

    let key = (entry_path.to_path_buf(), runtime);
    let mut memos = MEMOS
        .lock()
        .expect("full-package memo should not be poisoned");
    // Only shed a victim when this insert actually grows the map. Re-storing a key
    // that is already present would otherwise evict a live memo for nothing, and at
    // a steady 256 entries every refresh would ratchet the map down by one.
    if !memos.contains_key(&key) && memos.len() >= MAX_ENTRIES {
        let coldest = memos
            .iter()
            .min_by_key(|(_, memo)| memo.used_at)
            .map(|(key, _)| key.clone());
        if let Some(coldest) = coldest {
            memos.remove(&coldest);
        }
    }
    memos.insert(
        key,
        Memo {
            minified_len,
            fingerprints,
            generation,
            stamp: tick(),
            used_at: tick(),
        },
    );
}
