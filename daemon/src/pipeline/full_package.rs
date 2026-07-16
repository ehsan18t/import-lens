//! Memo for the full-package comparison build (§8.4/§6.3).
//!
//! `truly_treeshakeable` asks one question — is the named import materially smaller
//! than the whole package? — and answers it with a second complete Rolldown build
//! plus a second complete minify, of which only the minified *length* is ever used.
//! That answer does not depend on which names were imported, but the import cache key
//! does, so for N named variants of one entry the daemon paid 2N full builds.
//!
//! See [`crate::pipeline::build_memo`] for what makes this safe to cache.

use std::path::Path;
use std::sync::LazyLock;

use super::build_memo::BuildMemo;
use crate::cache::key::FileFingerprint;
use crate::ipc::protocol::ImportRuntime;

static MEMO: LazyLock<BuildMemo<u64>> = LazyLock::new(BuildMemo::new);

/// The memoized full-package minified length, if it is still valid.
pub(crate) fn lookup(entry_path: &Path, runtime: ImportRuntime) -> Option<u64> {
    MEMO.get(entry_path, runtime)
}

pub(crate) fn store(
    entry_path: &Path,
    runtime: ImportRuntime,
    minified_len: u64,
    fingerprints: Vec<FileFingerprint>,
    generation: u64,
) {
    MEMO.insert(entry_path, runtime, minified_len, fingerprints, generation);
}
