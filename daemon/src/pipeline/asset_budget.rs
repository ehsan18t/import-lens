//! One resource ledger for every non-JavaScript input and every CSS retry in a build.

use crate::cache::key::{
    FileFingerprint, read_time_len_mtime_of, sort_and_dedup_fingerprints,
    unverifiable_file_fingerprint,
};
use crate::engine::limits::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES};
use crate::engine::{AssetKind, CollectedAsset};
use crate::pipeline::asset_boundary::AssetDeadline;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_CSS_WORK_READS: usize = 512;
const MAX_CSS_WORK_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct AssetBudgetLimits {
    max_unique_files: usize,
    max_input_bytes: usize,
    max_file_bytes: usize,
    max_css_work_reads: usize,
    max_css_work_bytes: usize,
}

impl AssetBudgetLimits {
    /// Production limits with the build-wide CSS work ledger lifted, for a test that exercises the
    /// PER-ATTEMPT stylesheet-tree bound.
    ///
    /// The two cannot both be production in one test: a union that breaches the 256-file attempt
    /// bound needs N > 256 reads, and its per-sheet retry reads all N again, so the pair costs 2N
    /// against a build-wide ledger of 512. Any N that triggers the degradation also exhausts the
    /// ledger. That is real production behaviour and is recorded in known-issues; a test that wants
    /// to observe the degradation itself has to hold the other limit out of the way.
    #[cfg(test)]
    pub(crate) fn unbounded_css_work() -> Self {
        Self {
            max_css_work_reads: usize::MAX,
            max_css_work_bytes: usize::MAX,
            ..Self::production()
        }
    }

    pub(crate) fn production() -> Self {
        Self {
            max_unique_files: MAX_GRAPH_MODULES,
            max_input_bytes: *MAX_GRAPH_SOURCE_BYTES,
            max_file_bytes: MAX_MODULE_SOURCE_BYTES,
            max_css_work_reads: MAX_CSS_WORK_READS,
            max_css_work_bytes: MAX_CSS_WORK_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetBudgetStage {
    ModuleGraphLimit,
    Timeout,
}

impl AssetBudgetStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ModuleGraphLimit => crate::engine::stage::MODULE_GRAPH_LIMIT,
            Self::Timeout => crate::engine::stage::TIMEOUT,
        }
    }
}

/// A settled resource failure plus the exact observations needed to expire a durable rejection.
#[derive(Debug, Clone)]
pub(crate) struct AssetBudgetFailure {
    pub(crate) stage: AssetBudgetStage,
    pub(crate) message: String,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) read_time_fingerprints: Vec<FileFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(crate) struct AssetBudgetUsage {
    pub(crate) unique_files: usize,
    pub(crate) input_bytes: usize,
    pub(crate) css_work_reads: usize,
    pub(crate) css_work_bytes: usize,
    pub(crate) snapshots: usize,
}

#[derive(Debug)]
struct BudgetState {
    unique_files: BTreeMap<PathBuf, usize>,
    graph_files: BTreeSet<PathBuf>,
    aliases: BTreeMap<PathBuf, PathBuf>,
    input_bytes: usize,
    css_work_reads: usize,
    css_work_bytes: usize,
    snapshots: BTreeMap<PathBuf, CollectedAsset>,
    read_paths: BTreeSet<PathBuf>,
    fingerprints: Vec<FileFingerprint>,
    module_limit_message: Option<String>,
    timed_out: bool,
}

/// The metadata reservation that closes the stat/read race for a CSS child.
pub(crate) struct CssReadReservation {
    path: PathBuf,
    metadata: std::fs::Metadata,
    css_work_reserved_bytes: Option<usize>,
}

/// Shared by the union attempt, every per-sheet retry, and CSS `url()` discovery.
pub(crate) struct AssetProcessingContext {
    state: Mutex<BudgetState>,
    /// Serializes check-then-read for URL resources so one canonical path has one snapshot.
    snapshot_gate: Mutex<()>,
    deadline: AssetDeadline,
    limits: AssetBudgetLimits,
}

impl AssetProcessingContext {
    pub(crate) fn new(
        graph_source_bytes: usize,
        graph_loaded_paths: &[PathBuf],
        direct_assets: &[CollectedAsset],
        deadline: AssetDeadline,
        limits: AssetBudgetLimits,
    ) -> Self {
        let mut graph_files = graph_loaded_paths.iter().cloned().collect::<BTreeSet<_>>();
        graph_files.extend(direct_assets.iter().map(|asset| asset.path.clone()));
        let mut unique_files = graph_files
            .iter()
            .cloned()
            .map(|path| (path, 0))
            .collect::<BTreeMap<_, _>>();
        let snapshots = direct_assets
            .iter()
            .cloned()
            .map(|asset| (asset.path.clone(), asset))
            .collect::<BTreeMap<_, _>>();
        for asset in snapshots.values() {
            unique_files
                .entry(asset.path.clone())
                .or_insert(asset.bytes().len());
        }

        let mut state = BudgetState {
            unique_files,
            graph_files,
            aliases: direct_assets
                .iter()
                .map(|asset| (asset.path.clone(), asset.path.clone()))
                .collect(),
            input_bytes: graph_source_bytes,
            css_work_reads: 0,
            css_work_bytes: 0,
            snapshots,
            read_paths: BTreeSet::new(),
            fingerprints: direct_assets
                .iter()
                .map(|asset| asset.fingerprint.clone())
                .collect(),
            module_limit_message: None,
            timed_out: false,
        };
        if state.unique_files.len() > limits.max_unique_files {
            state.module_limit_message = Some(format!(
                "asset processing exceeds the {} file graph limit",
                limits.max_unique_files
            ));
        }
        if state.input_bytes > limits.max_input_bytes {
            record_limit(
                &mut state,
                format!(
                    "asset processing exceeds the {} byte total source limit",
                    limits.max_input_bytes
                ),
            );
        }

        Self {
            state: Mutex::new(state),
            snapshot_gate: Mutex::new(()),
            deadline,
            limits,
        }
    }

    pub(crate) fn production(
        graph_source_bytes: usize,
        graph_loaded_paths: &[PathBuf],
        direct_assets: &[CollectedAsset],
        deadline: AssetDeadline,
    ) -> Self {
        Self::new(
            graph_source_bytes,
            graph_loaded_paths,
            direct_assets,
            deadline,
            AssetBudgetLimits::production(),
        )
    }

    pub(crate) fn check_deadline(&self) -> std::io::Result<()> {
        let mut state = self.lock_state();
        self.check_available(&mut state)
    }

    /// Charge an already captured stylesheet every time a union/retry provider consumes it.
    pub(crate) fn charge_css_snapshot(&self, asset: &CollectedAsset) -> std::io::Result<()> {
        let mut state = self.lock_state();
        self.check_available(&mut state)?;
        state.read_paths.insert(asset.path.clone());
        state.fingerprints.push(asset.fingerprint.clone());
        self.charge_css_work(&mut state, asset.bytes().len(), &asset.path)
    }

    /// Reserve metadata length and CSS work before Lightning CSS opens a child stylesheet.
    pub(crate) fn begin_css_read(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> std::io::Result<CssReadReservation> {
        let path = canonical_path(path);
        let metadata_bytes = metadata_bytes(metadata, &path)?;
        let mut state = self.lock_state();
        self.check_available(&mut state)?;
        state.read_paths.insert(path.clone());
        state.fingerprints.push(stat_fingerprint(&path, metadata));
        self.charge_css_work(&mut state, metadata_bytes, &path)?;
        self.reserve_unique(&mut state, &path, metadata_bytes)?;
        Ok(CssReadReservation {
            path,
            metadata: metadata.clone(),
            css_work_reserved_bytes: Some(metadata_bytes),
        })
    }

    /// Bind the exact child bytes to the reservation and retain them for later retry providers.
    pub(crate) fn finish_css_read(
        &self,
        reservation: CssReadReservation,
        bytes: &[u8],
    ) -> std::io::Result<CollectedAsset> {
        self.finish_read(reservation, AssetKind::Css, bytes)
    }

    /// Read a CSS-discovered font/wasm once, under metadata-first resource admission.
    pub(crate) fn snapshot(&self, path: &Path, kind: AssetKind) -> std::io::Result<CollectedAsset> {
        let requested_path = path.to_path_buf();
        let _gate = self
            .snapshot_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(asset) = self.snapshot_if_present(&requested_path) {
            self.check_deadline()?;
            return Ok(asset);
        }
        let path = canonical_path(&requested_path);
        self.lock_state()
            .aliases
            .insert(requested_path, path.clone());

        self.check_deadline()?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.record_failed_path(&path);
                return Err(error);
            }
        };
        let reservation = self.begin_resource_read(&path, &metadata)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.record_failed_path(&path);
                return Err(error);
            }
        };
        self.finish_read(reservation, kind, &bytes)
    }

    pub(crate) fn snapshot_if_present(&self, path: &Path) -> Option<CollectedAsset> {
        {
            let state = self.lock_state();
            let identity = state.aliases.get(path).map_or(path, PathBuf::as_path);
            if let Some(asset) = state.snapshots.get(identity) {
                return Some(asset.clone());
            }
        }
        let canonical = canonical_path(path);
        self.lock_state().snapshots.get(&canonical).cloned()
    }

    /// One snapshot by canonical path, for a retry that needs the bytes an earlier attempt read.
    ///
    /// Deliberately not a bulk `snapshots()` accessor. Handing out the whole map made every
    /// per-sheet retry clone every read before it — quadratic in exactly the degraded case the
    /// retries exist to serve — and no caller ever needed more than the one path it was about to
    /// open.
    pub(crate) fn snapshot_for(&self, path: &Path) -> Option<CollectedAsset> {
        self.lock_state().snapshots.get(path).cloned()
    }

    pub(crate) fn read_paths(&self) -> Vec<PathBuf> {
        self.lock_state().read_paths.iter().cloned().collect()
    }

    pub(crate) fn freshness_fingerprints(&self) -> Vec<FileFingerprint> {
        normalized_observations(&self.lock_state().fingerprints)
    }

    pub(crate) fn record_failed_path(&self, path: &Path) {
        let path = canonical_path(path);
        let mut state = self.lock_state();
        state.read_paths.insert(path.clone());
        state.fingerprints.push(unverifiable_file_fingerprint(path));
    }

    pub(crate) fn failure(&self) -> Option<AssetBudgetFailure> {
        let state = self.lock_state();
        let (stage, message) = if let Some(message) = &state.module_limit_message {
            (AssetBudgetStage::ModuleGraphLimit, message.clone())
        } else if state.timed_out {
            (
                AssetBudgetStage::Timeout,
                "asset processing did not complete within its deadline".to_owned(),
            )
        } else {
            return None;
        };
        let read_time_fingerprints = normalized_observations(&state.fingerprints);
        Some(AssetBudgetFailure {
            stage,
            message,
            read_paths: state.read_paths.iter().cloned().collect(),
            read_time_fingerprints,
        })
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> AssetBudgetUsage {
        let state = self.lock_state();
        AssetBudgetUsage {
            unique_files: state.unique_files.len(),
            input_bytes: state.input_bytes,
            css_work_reads: state.css_work_reads,
            css_work_bytes: state.css_work_bytes,
            snapshots: state.snapshots.len(),
        }
    }

    fn begin_resource_read(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> std::io::Result<CssReadReservation> {
        let metadata_bytes = metadata_bytes(metadata, path)?;
        let mut state = self.lock_state();
        self.check_available(&mut state)?;
        state.read_paths.insert(path.to_path_buf());
        state.fingerprints.push(stat_fingerprint(path, metadata));
        self.reserve_unique(&mut state, path, metadata_bytes)?;
        Ok(CssReadReservation {
            path: path.to_path_buf(),
            metadata: metadata.clone(),
            css_work_reserved_bytes: None,
        })
    }

    fn finish_read(
        &self,
        reservation: CssReadReservation,
        kind: AssetKind,
        bytes: &[u8],
    ) -> std::io::Result<CollectedAsset> {
        let asset = CollectedAsset::from_read(
            reservation.path.clone(),
            kind,
            &reservation.metadata,
            bytes.to_vec(),
        );
        let actual_bytes = bytes.len();
        let mut state = self.lock_state();
        self.check_available(&mut state)?;
        state.read_paths.insert(reservation.path.clone());
        state.fingerprints.push(asset.fingerprint.clone());

        if let Some(reserved_bytes) = reservation.css_work_reserved_bytes {
            let without_reservation = state
                .css_work_bytes
                .checked_sub(reserved_bytes)
                .expect("CSS work bytes must have an existing reservation");
            let reconciled_work_bytes = without_reservation.saturating_add(actual_bytes);
            if reconciled_work_bytes > self.limits.max_css_work_bytes {
                record_limit(
                    &mut state,
                    format!(
                        "asset processing exceeds the {} CSS read / {} CSS byte work limit while reading {}",
                        self.limits.max_css_work_reads,
                        self.limits.max_css_work_bytes,
                        reservation.path.display()
                    ),
                );
                return Err(limit_io_error(&state));
            }
            state.css_work_bytes = reconciled_work_bytes;
        }

        if actual_bytes > self.limits.max_file_bytes {
            record_limit(
                &mut state,
                format!(
                    "asset {} exceeds the {} byte module source limit",
                    reservation.path.display(),
                    self.limits.max_file_bytes
                ),
            );
            return Err(limit_io_error(&state));
        }

        // Every successful observation reconciles against the path ledger, not only the read that
        // first inserted it. A union attempt may reserve metadata and fail locally before reading;
        // a later per-sheet retry then sees an existing path. If that file grew, skipping the
        // second reconciliation would let the growth escape the aggregate cap. Keep the maximum
        // observed size: overlapping reads can briefly retain both snapshots, so shrinking the
        // reservation here would understate peak memory.
        if !state.graph_files.contains(&reservation.path) {
            let charged_bytes = *state
                .unique_files
                .get(&reservation.path)
                .expect("a completed asset read must have a metadata reservation");
            let additional_bytes = actual_bytes.saturating_sub(charged_bytes);
            let reconciled = state.input_bytes.saturating_add(additional_bytes);
            if reconciled > self.limits.max_input_bytes {
                record_limit(
                    &mut state,
                    format!(
                        "asset processing exceeds the {} byte total source limit while reading {}",
                        self.limits.max_input_bytes,
                        reservation.path.display()
                    ),
                );
                return Err(limit_io_error(&state));
            }
            state.input_bytes = reconciled;
            if actual_bytes > charged_bytes {
                state
                    .unique_files
                    .insert(reservation.path.clone(), actual_bytes);
            }
        }

        state
            .snapshots
            .entry(reservation.path)
            .or_insert_with(|| asset.clone());
        Ok(asset)
    }

    fn reserve_unique(
        &self,
        state: &mut BudgetState,
        path: &Path,
        bytes: usize,
    ) -> std::io::Result<()> {
        if bytes > self.limits.max_file_bytes {
            record_limit(
                state,
                format!(
                    "asset {} exceeds the {} byte module source limit",
                    path.display(),
                    self.limits.max_file_bytes
                ),
            );
            return Err(limit_io_error(state));
        }
        if state.unique_files.contains_key(path) {
            return Ok(());
        }
        if state.unique_files.len().saturating_add(1) > self.limits.max_unique_files {
            record_limit(
                state,
                format!(
                    "asset processing exceeds the {} file graph limit while loading {}",
                    self.limits.max_unique_files,
                    path.display()
                ),
            );
            return Err(limit_io_error(state));
        }
        let next_input_bytes = state.input_bytes.saturating_add(bytes);
        if next_input_bytes > self.limits.max_input_bytes {
            record_limit(
                state,
                format!(
                    "asset processing exceeds the {} byte total source limit while loading {}",
                    self.limits.max_input_bytes,
                    path.display()
                ),
            );
            return Err(limit_io_error(state));
        }
        state.unique_files.insert(path.to_path_buf(), bytes);
        state.input_bytes = next_input_bytes;
        Ok(())
    }

    fn charge_css_work(
        &self,
        state: &mut BudgetState,
        bytes: usize,
        path: &Path,
    ) -> std::io::Result<()> {
        let reads = state.css_work_reads.saturating_add(1);
        let next_css_work_bytes = state.css_work_bytes.saturating_add(bytes);
        if reads > self.limits.max_css_work_reads
            || next_css_work_bytes > self.limits.max_css_work_bytes
        {
            record_limit(
                state,
                format!(
                    "asset processing exceeds the {} CSS read / {} CSS byte work limit while loading {}",
                    self.limits.max_css_work_reads,
                    self.limits.max_css_work_bytes,
                    path.display()
                ),
            );
            return Err(limit_io_error(state));
        }
        state.css_work_reads = reads;
        state.css_work_bytes = next_css_work_bytes;
        Ok(())
    }

    fn check_available(&self, state: &mut BudgetState) -> std::io::Result<()> {
        if state.module_limit_message.is_some() {
            return Err(limit_io_error(state));
        }
        if self.deadline.is_expired() {
            state.timed_out = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "asset processing deadline expired",
            ));
        }
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BudgetState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn metadata_bytes(metadata: &std::fs::Metadata, path: &Path) -> std::io::Result<usize> {
    usize::try_from(metadata.len()).map_err(|_| {
        std::io::Error::other(format!(
            "asset {} is too large for this platform",
            path.display()
        ))
    })
}

fn stat_fingerprint(path: &Path, metadata: &std::fs::Metadata) -> FileFingerprint {
    let (len, modified_millis) = read_time_len_mtime_of(metadata);
    FileFingerprint {
        path: path.to_string_lossy().replace('\\', "/"),
        len,
        modified_millis,
        content_hash: None,
    }
}

/// Drop only the stat observation made redundant by an exact observation of the SAME metadata.
/// Stat/exact pairs with different metadata must both survive: they prove the file changed between
/// attempts, making the assembled result non-reusable.
fn normalized_observations(observations: &[FileFingerprint]) -> Vec<FileFingerprint> {
    let exact_metadata = observations
        .iter()
        .filter(|fingerprint| fingerprint.content_hash.is_some())
        .map(|fingerprint| {
            (
                fingerprint.path.clone(),
                fingerprint.len,
                fingerprint.modified_millis,
            )
        })
        .collect::<BTreeSet<_>>();
    let mut fingerprints = observations
        .iter()
        .filter(|fingerprint| {
            fingerprint.content_hash.is_some()
                || !exact_metadata.contains(&(
                    fingerprint.path.clone(),
                    fingerprint.len,
                    fingerprint.modified_millis,
                ))
        })
        .cloned()
        .collect::<Vec<_>>();
    sort_and_dedup_fingerprints(&mut fingerprints);
    fingerprints
}

/// Keep the FIRST breach, which is the one that actually stopped the work.
///
/// This used to keep the lexicographically smallest message, so the reported limit could name a
/// later breach that only happened because the first one had already been hit — telling the user
/// about a symptom while the cause sat behind it. Ordering by content also made the answer depend on
/// how the messages happened to be spelled.
fn record_limit(state: &mut BudgetState, message: String) {
    state.module_limit_message.get_or_insert(message);
}

fn limit_io_error(state: &BudgetState) -> std::io::Error {
    std::io::Error::other(
        state
            .module_limit_message
            .clone()
            .unwrap_or_else(|| "asset processing resource limit exceeded".to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::read_collected_asset;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "il-asset-budget-{}-{tag}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn limits() -> AssetBudgetLimits {
        AssetBudgetLimits {
            max_unique_files: 4,
            max_input_bytes: 16,
            max_file_bytes: 12,
            max_css_work_reads: 2,
            max_css_work_bytes: 8,
        }
    }

    fn deadline() -> AssetDeadline {
        AssetDeadline::for_test(Duration::from_secs(5))
    }

    #[test]
    fn metadata_breach_refuses_a_snapshot_and_keeps_the_offending_stat() {
        let dir = temp_dir("preflight");
        let font = dir.join("large.woff2");
        fs::write(&font, [0x31; 13]).expect("font");
        let context = AssetProcessingContext::new(4, &[], &[], deadline(), limits());

        assert!(context.snapshot(&font, AssetKind::Font).is_err());
        let failure = context.failure().expect("typed budget failure");
        assert_eq!(failure.stage, AssetBudgetStage::ModuleGraphLimit);
        assert!(failure.message.contains("12 byte module source limit"));
        assert!(
            failure
                .read_time_fingerprints
                .iter()
                .any(|fingerprint| fingerprint.len == 13 && fingerprint.content_hash.is_none())
        );
        assert_eq!(context.usage().snapshots, 0);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn canonical_snapshot_is_read_once_and_survives_a_later_delete() {
        let dir = temp_dir("snapshot");
        let font = dir.join("same.woff2");
        fs::write(&font, [0x52; 4]).expect("font");
        let context = AssetProcessingContext::new(0, &[], &[], deadline(), limits());

        let first = context
            .snapshot(&font, AssetKind::Font)
            .expect("first read");
        fs::remove_file(&font).expect("delete after snapshot");
        let second = context
            .snapshot(&font, AssetKind::Font)
            .expect("cached read");

        assert_eq!(first, second);
        assert_eq!(context.usage().snapshots, 1);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn css_work_is_shared_across_attempts_instead_of_resetting() {
        let dir = temp_dir("css-work");
        let css = dir.join("entry.css");
        fs::write(&css, "a{}").expect("css");
        let asset = read_collected_asset(&css, AssetKind::Css).expect("snapshot");
        let context = AssetProcessingContext::new(
            asset.bytes().len(),
            std::slice::from_ref(&asset.path),
            std::slice::from_ref(&asset),
            deadline(),
            limits(),
        );

        context.charge_css_snapshot(&asset).expect("union read");
        context.charge_css_snapshot(&asset).expect("retry read");
        assert!(context.charge_css_snapshot(&asset).is_err());

        let usage = context.usage();
        assert_eq!(usage.css_work_reads, 2);
        assert_eq!(usage.css_work_bytes, 6);
        assert_eq!(
            context.failure().expect("typed work failure").stage,
            AssetBudgetStage::ModuleGraphLimit
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_retry_reconciles_growth_against_an_earlier_path_reservation() {
        let dir = temp_dir("retry-growth");
        let css = dir.join("child.css");
        fs::write(&css, [b'a'; 4]).expect("initial child");
        let limits = AssetBudgetLimits {
            max_unique_files: 4,
            max_input_bytes: 10,
            max_file_bytes: 12,
            max_css_work_reads: 4,
            max_css_work_bytes: 64,
        };
        let context = AssetProcessingContext::new(0, &[], &[], deadline(), limits);

        let first_metadata = fs::metadata(&css).expect("initial metadata");
        let _abandoned = context
            .begin_css_read(&css, &first_metadata)
            .expect("the union reserves the initial length");

        fs::write(&css, [b'b'; 12]).expect("grow before retry");
        let retry_metadata = fs::metadata(&css).expect("retry metadata");
        let retry = context
            .begin_css_read(&css, &retry_metadata)
            .expect("the existing path reaches exact reconciliation");
        assert!(
            context.finish_css_read(retry, &[b'b'; 12]).is_err(),
            "the retry's growth must not escape through an existing path entry"
        );
        let failure = context.failure().expect("typed aggregate failure");
        assert_eq!(failure.stage, AssetBudgetStage::ModuleGraphLimit);
        assert!(failure.message.contains("10 byte total source limit"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn css_work_reconciles_metadata_to_the_exact_read_length() {
        let dir = temp_dir("css-work-growth");
        let css = dir.join("child.css");
        fs::write(&css, [b'a'; 1]).expect("initial child");
        let limits = AssetBudgetLimits {
            max_unique_files: 4,
            max_input_bytes: 64,
            max_file_bytes: 32,
            max_css_work_reads: 4,
            max_css_work_bytes: 5,
        };
        let context = AssetProcessingContext::new(0, &[], &[], deadline(), limits);
        let metadata = fs::metadata(&css).expect("metadata");
        let reservation = context
            .begin_css_read(&css, &metadata)
            .expect("metadata fits the CSS work budget");

        assert!(
            context.finish_css_read(reservation, &[b'b'; 6]).is_err(),
            "read-time growth must be charged to the cumulative CSS work ledger"
        );
        assert!(
            context
                .failure()
                .expect("typed CSS work failure")
                .message
                .contains("5 CSS byte work limit")
        );
        fs::remove_dir_all(dir).ok();
    }
}
