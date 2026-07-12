//! Memo for export enumeration (§8.4).
//!
//! Completion asks a package "what do you export?", and the daemon answered with a
//! full, uncached Rolldown build of the entire package graph — on every popup, for a
//! list that only changes when the package's files do.
//!
//! See [`crate::pipeline::build_memo`] for what makes this safe to cache. The
//! enumeration's own read-time fingerprints describe exactly the files the export list
//! was derived from, so it expires precisely when that list would have gone wrong.

use std::path::Path;
use std::sync::LazyLock;

use super::build_memo::BuildMemo;
use crate::engine::{BundleFailure, EngineBudget, ExportEnumeration, boundary};
use crate::ipc::protocol::ImportRuntime;

static MEMO: LazyLock<BuildMemo<ExportEnumeration>> = LazyLock::new(BuildMemo::new);

/// Enumerate a package entry's exports, reusing a previous build's answer while every
/// file it was derived from is unchanged.
///
/// `budget` is the calling request's engine budget (§9): a memo hit costs nothing, but a miss is
/// a full package-graph build, and the completion popup that asked for it is on the same 10s
/// deadline as everything else the client waits for.
pub fn enumerate_exports_cached(
    entry_path: &Path,
    runtime: ImportRuntime,
    budget: EngineBudget,
) -> Result<ExportEnumeration, BundleFailure> {
    if let Some(cached) = MEMO.get(entry_path, runtime) {
        return Ok(cached);
    }

    // Read before the build, not after: an invalidation landing while the build is in
    // flight must not be stamped onto a list derived from the bytes it invalidated.
    let generation = crate::cache::memory::cache_generation();
    let enumeration = boundary::enumerate_exports_sync(entry_path.to_path_buf(), runtime, budget)?;

    // A graph carrying a module the plugin could not fingerprint as it read it has no
    // complete read-time record, so there is nothing to expire a memo against.
    if enumeration.unhashed_paths.is_empty() {
        MEMO.insert(
            entry_path,
            runtime,
            enumeration.clone(),
            enumeration.read_time_fingerprints.clone(),
            generation,
        );
    }

    Ok(enumeration)
}
