//! Memo for export enumeration (§8.4).
//!
//! Completion asks a package "what do you export?", and the daemon answered with a
//! full, uncached Rolldown build of the entire package graph — on every popup, for a
//! list that only changes when the package's files do.
//!
//! See [`crate::pipeline::build_memo`] for what makes this safe to cache. The enumeration
//! is fingerprinted against exactly the freshness set the size path uses — its own
//! read-time module fingerprints PLUS the package and first-party manifests
//! (`analyze::manifest_augmented_fingerprints`, §8.3) — so it expires precisely when the
//! export list would have gone wrong. Fingerprinting only the source modules left a
//! first-party `package.json` edit (its `type`, `exports`, or `sideEffects`) serving the
//! old list forever, since that edit moves no source file.

use std::path::Path;
use std::sync::LazyLock;

use super::build_memo::BuildMemo;
use crate::engine::{BundleFailure, ExportEnumeration, boundary};
use crate::ipc::protocol::ImportRuntime;
use crate::pipeline::analyze::{AnalysisContext, manifest_augmented_fingerprints};

static MEMO: LazyLock<BuildMemo<ExportEnumeration>> = LazyLock::new(BuildMemo::new);

/// Enumerate a package entry's exports, reusing a previous build's answer while every
/// file it was derived from is unchanged.
///
/// A memo hit costs nothing; a miss is a full package-graph build, bounded — like every other
/// build — by `boundary::BUILD_TIMEOUT`. Only a *successful* enumeration is memoized, so a build
/// that timed out or panicked leaves nothing behind to be served as if it were an answer.
pub fn enumerate_exports_cached(
    context: &AnalysisContext,
    package_root: &Path,
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<ExportEnumeration, BundleFailure> {
    if let Some(cached) = MEMO.get(entry_path, runtime) {
        return Ok(cached);
    }

    // Read before the build, not after: an invalidation landing while the build is in
    // flight must not be stamped onto a list derived from the bytes it invalidated.
    let generation = crate::cache::memory::cache_generation();
    let enumeration = boundary::enumerate_exports_sync(entry_path.to_path_buf(), runtime)?;

    // A graph carrying a module the plugin could not fingerprint as it read it has no
    // complete read-time record, so there is nothing to expire a memo against.
    if enumeration.unhashed_paths.is_empty() {
        let fingerprints = manifest_augmented_fingerprints(
            context,
            package_root,
            &enumeration.read_time_fingerprints,
            &enumeration.loaded_paths,
        );
        MEMO.insert(
            entry_path,
            runtime,
            enumeration.clone(),
            fingerprints,
            generation,
        );
    }

    Ok(enumeration)
}
