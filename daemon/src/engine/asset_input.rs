use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cache::key::{
    FileFingerprint, content_hash, file_fingerprint_from_read_time, read_time_len_mtime_of,
};

use super::AssetKind;

/// An immutable non-JavaScript input captured at the engine load boundary.
///
/// The bytes and fingerprint are deliberately one value: post-build processing must never reopen
/// `path` and accidentally bind a size from new bytes to the fingerprint of the old bytes. `Arc`
/// keeps clones cheap while the build state and translated artifact briefly share ownership.
#[derive(Clone, PartialEq, Eq)]
pub struct CollectedAsset {
    pub path: PathBuf,
    pub kind: AssetKind,
    bytes: Arc<[u8]>,
    pub fingerprint: FileFingerprint,
}

impl CollectedAsset {
    pub(super) fn from_read(
        canonical_path: PathBuf,
        kind: AssetKind,
        metadata: &std::fs::Metadata,
        bytes: Vec<u8>,
    ) -> Self {
        let (len, modified_millis) = read_time_len_mtime_of(metadata);
        let fingerprint = file_fingerprint_from_read_time(
            &canonical_path,
            len,
            modified_millis,
            content_hash(&bytes),
        );
        Self {
            path: canonical_path,
            kind,
            bytes: Arc::from(bytes),
            fingerprint,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn raw_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

impl fmt::Debug for CollectedAsset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CollectedAsset")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("raw_bytes", &self.raw_bytes())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Read one processor-discovered asset once and bind its fingerprint to those exact bytes.
/// Canonicalization happens before the stat/read pair so a symlink retarget cannot give the bytes
/// of one target the identity of another.
pub(crate) fn read_collected_asset(
    path: &Path,
    kind: AssetKind,
) -> std::io::Result<CollectedAsset> {
    let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let metadata = std::fs::metadata(&canonical_path)?;
    let bytes = std::fs::read(&canonical_path)?;
    Ok(CollectedAsset::from_read(
        canonical_path,
        kind,
        &metadata,
        bytes,
    ))
}
