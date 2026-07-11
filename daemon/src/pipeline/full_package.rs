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
    used_at: u64,
}

static MEMOS: LazyLock<Mutex<HashMap<(PathBuf, ImportRuntime), Memo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TICK: AtomicU64 = AtomicU64::new(0);

fn tick() -> u64 {
    TICK.fetch_add(1, Ordering::Relaxed)
}

/// The memoized full-package minified length, if one was stored for this entry
/// and every file it was measured from is still current. A stale or vanished
/// dependency drops the memo, so the caller rebuilds.
pub(crate) fn lookup(entry_path: &Path, runtime: ImportRuntime) -> Option<u64> {
    let key = (entry_path.to_path_buf(), runtime);
    let fingerprints = {
        let memos = MEMOS
            .lock()
            .expect("full-package memo should not be poisoned");
        memos.get(&key)?.fingerprints.clone()
    };

    // Never hold the lock across the freshness check: it stats, and may read and
    // hash, every module in the package graph.
    if check_fingerprints_strict(&fingerprints) != Freshness::Fresh {
        MEMOS
            .lock()
            .expect("full-package memo should not be poisoned")
            .remove(&key);
        return None;
    }

    let mut memos = MEMOS
        .lock()
        .expect("full-package memo should not be poisoned");
    let memo = memos.get_mut(&key)?;
    memo.used_at = tick();
    Some(memo.minified_len)
}

/// Store a full-package length against the fingerprints of the exact bytes it was
/// measured from. Storing nothing is always safe — the caller just rebuilds.
pub(crate) fn store(
    entry_path: &Path,
    runtime: ImportRuntime,
    minified_len: u64,
    fingerprints: Vec<FileFingerprint>,
) {
    if fingerprints.is_empty() {
        return;
    }

    let mut memos = MEMOS
        .lock()
        .expect("full-package memo should not be poisoned");
    if memos.len() >= MAX_ENTRIES {
        let coldest = memos
            .iter()
            .min_by_key(|(_, memo)| memo.used_at)
            .map(|(key, _)| key.clone());
        if let Some(coldest) = coldest {
            memos.remove(&coldest);
        }
    }
    memos.insert(
        (entry_path.to_path_buf(), runtime),
        Memo {
            minified_len,
            fingerprints,
            used_at: tick(),
        },
    );
}
