//! Non-JS asset processing (B2): a package's real cost is not only its JavaScript. A UI kit ships
//! CSS; some packages ship wasm or fonts. The engine measures the JS chunk exactly and hands the
//! reachable assets here to be processed the way they actually ship, so their bytes can be folded
//! into the Import Cost rather than merely disclosed.
//!
//! - **CSS** goes through Lightning CSS: resolve the `@import` tree from disk into one stylesheet,
//!   minify, print. Every reachable stylesheet becomes ONE artifact, mirroring how CSS ships (a
//!   single file per entry) and how the esbuild oracle emits a single `.css` beside the JS chunk,
//!   which also lets Lightning CSS dedupe what they share.
//! - **wasm / fonts** have no processor; their shipped size is the raw file bytes (woff2 is already
//!   brotli-internally, so it barely shrinks, which is correct).
//!
//! Each artifact is compressed **on its own** and the sizes are summed
//! ([ADR-0005](../../../docs/adr/0005-a-runtime-is-an-artifact-boundary.md)): they are separate
//! files that ship separately, so concatenating them before compressing would invent a number.
//!
//! Every path Lightning CSS opens — the entry and each resolved `@import` child — plus supported
//! local artifacts referenced by `url()` are captured for cache freshness, so an edit to any of
//! them invalidates the measured size. Any processing failure falls back to disclosing the raw
//! bytes, which is exactly today's behaviour: never below it
//! ([ADR-0006](../../../docs/adr/0006-the-result-model.md)).

use crate::cache::key::{FileFingerprint, sort_and_dedup_fingerprints};
#[cfg(test)]
use crate::engine::read_collected_asset;
use crate::engine::{AssetKind, CollectedAsset, UncountedAsset, diagnostic_stage};
use crate::ipc::protocol::{AssetContribution, ImportDiagnostic, MeasuredSizes};
use crate::pipeline::asset_boundary::{self, AssetBoundaryError, AssetDeadline};
#[cfg(test)]
use crate::pipeline::asset_budget::AssetBudgetLimits;
use crate::pipeline::asset_budget::{AssetBudgetFailure, AssetProcessingContext};
use crate::pipeline::compress::{CompressionSizes, compress_all_bytes};
use crate::pipeline::css_dependencies::collect_referenced_assets;
use lightningcss::bundler::{Bundler, FileProvider, ResolveResult, SourceProvider};
use lightningcss::dependencies::DependencyOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions};
use lightningcss::targets::Targets;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many files one `@import` tree may pull in, and how many bytes of them.
///
/// The JavaScript graph has [`crate::engine::limits`]; a stylesheet's `@import` children are never
/// graph modules, so nothing bounded them at all.
///
/// The file count doubles as the DEPTH bound, which is what makes it load-bearing rather than tidy.
/// Lightning CSS recurses per `@import`, and a chain deep enough overflows the stack — around 800
/// frames in a release build. That is not catchable: `catch_unwind` never runs, the process
/// `__fastfail`s, and the daemon dies with every in-flight request. The canonicalizing `resolve`
/// below removes the cycle that made depth unbounded; this bounds the honest-but-absurd chain that
/// remains. A chain of N files costs N reads, so refusing at 256 stops the walk roughly three times
/// short of where the stack gives out in the build that actually ships.
///
/// Breaching either is not a wrong number: the set falls back to the per-sheet path, and failing that
/// to raw-byte disclosure, which is the pre-B2 behaviour and the floor this feature promised never to
/// go below.
///
/// It cannot be raised on the grounds that a flat set of many sheets is harmless: the budget cannot
/// tell breadth from depth, and giving the walk its own big stack does not help, because Lightning CSS
/// drives the `@import` graph on `rayon` workers, whose stacks it does not own. 256 stylesheets in one
/// runtime group is already far more than real packages ship; a set past it degrades to the per-sheet
/// path, which is disclosed, rather than being dropped.
const MAX_STYLESHEET_FILES: usize = 256;
const MAX_STYLESHEET_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Default)]
struct ReadBudget {
    files: usize,
    bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct ReadReservation {
    bytes: usize,
}

/// Append-only ownership for stylesheet strings read by [`TrackingProvider`]. Lightning CSS's
/// `SourceProvider` returns `&str`, so every returned allocation must stay at a stable address until
/// the provider is dropped. This is the same ownership model as Lightning CSS's `FileProvider`, but
/// accepting bytes here lets us fingerprint the exact read before classifying invalid UTF-8.
#[derive(Default)]
struct RetainedSources {
    inputs: Mutex<Vec<*mut String>>,
    /// Snapshot bytes kept alive for as long as the provider is, so a preloaded stylesheet can be
    /// handed to Lightning CSS by reference instead of being copied into the arena above.
    snapshots: Mutex<Vec<Arc<[u8]>>>,
}

// SAFETY: pointers are inserted once behind the mutex, point to independently boxed strings, and
// are exposed only as immutable `&str`. They are never removed or mutated until `Drop`, which
// cannot run while a borrow of the provider is live.
unsafe impl Send for RetainedSources {}
unsafe impl Sync for RetainedSources {}

impl RetainedSources {
    fn retain(&self, source: String) -> &str {
        let pointer = Box::into_raw(Box::new(source));
        self.inputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(pointer);
        // SAFETY: `pointer` remains owned by this append-only collection until `Drop`; the boxed
        // allocation is stable even if the pointer vector reallocates.
        unsafe { &*pointer }
    }

    /// Borrow an already-read snapshot's bytes for the provider's lifetime, without copying them.
    fn retain_snapshot(&self, bytes: Arc<[u8]>) -> &[u8] {
        let pointer: *const [u8] = Arc::as_ptr(&bytes);
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(bytes);
        // SAFETY: the `Arc` clone is moved into this append-only collection and dropped only with
        // the provider, so the buffer outlives every borrow handed out here. The pointee is the
        // Arc's own heap allocation, which does not move when the holding vector reallocates.
        unsafe { &*pointer }
    }
}

impl Drop for RetainedSources {
    fn drop(&mut self) {
        let pointers = self
            .inputs
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for pointer in pointers.drain(..) {
            // SAFETY: every pointer was created exactly once by `retain` and is drained exactly
            // once here, after no borrow of the provider can remain.
            drop(unsafe { Box::from_raw(pointer) });
        }
    }
}

/// A `SourceProvider` that reads from disk like the built-in `FileProvider` but records every path
/// it opens, bounds what one `@import` tree may pull in, and can serve one synthetic in-memory
/// entry. The bundler drives the `@import` graph with `rayon`, so the provider must be
/// `Send + Sync`; its state is behind `Mutex`, never a `RefCell`.
struct TrackingProvider {
    inner: FileProvider,
    retained_sources: RetainedSources,
    /// Top-level stylesheets already captured by the engine. Serving these bytes makes a
    /// `BundleArtifact` immutable: processing never reopens an entry behind its fingerprint.
    preloaded: BTreeMap<PathBuf, CollectedAsset>,
    /// A virtual entry that `@import`s each reachable stylesheet by absolute path, so N stylesheets
    /// bundle into ONE artifact. `None` when there is a single real entry to bundle directly.
    synthetic: Option<(PathBuf, String)>,
    read_paths: Mutex<HashSet<PathBuf>>,
    read_time_fingerprints: Mutex<Vec<FileFingerprint>>,
    failed_paths: Mutex<Vec<FailedRead>>,
    budget: Mutex<ReadBudget>,
    /// One ledger for every union/per-sheet attempt in this build. Never optional: production
    /// safety that a caller can omit is safety the tests will omit.
    context: Arc<AssetProcessingContext>,
}

impl TrackingProvider {
    /// The ONE way to build a provider.
    ///
    /// There were four: bounded and unbounded, each with and without a synthetic entry. The
    /// unbounded pair existed only so processor tests could skip the ledger — which meant the
    /// tests exercised a different code path from production, and the safety context was optional
    /// exactly where it was load-bearing. Tests now pass a context with test limits instead.
    ///
    /// `preloaded` holds only THIS attempt's entries. Everything an earlier attempt read is looked
    /// up on demand through the context, because copying the whole snapshot map per attempt made
    /// each per-sheet retry pay for every read before it.
    fn new(
        entries: &[CollectedAsset],
        synthetic: Option<(PathBuf, String)>,
        context: Arc<AssetProcessingContext>,
    ) -> Self {
        Self {
            inner: FileProvider::new(),
            retained_sources: RetainedSources::default(),
            preloaded: entries
                .iter()
                .cloned()
                .map(|asset| (asset.path.clone(), asset))
                .collect(),
            synthetic,
            read_paths: Mutex::new(HashSet::new()),
            read_time_fingerprints: Mutex::new(Vec::new()),
            failed_paths: Mutex::new(Vec::new()),
            budget: Mutex::new(ReadBudget::default()),
            context,
        }
    }

    /// Reserve one file against the tree's budget before its bytes are read.
    fn reserve(&self, bytes: usize) -> Result<ReadReservation, std::io::Error> {
        let mut budget = self
            .budget
            .lock()
            .expect("css read budget should not be poisoned");
        let files = budget.files.saturating_add(1);
        let next_bytes = budget.bytes.saturating_add(bytes);
        if files > MAX_STYLESHEET_FILES || next_bytes > MAX_STYLESHEET_BYTES {
            return Err(std::io::Error::other(format!(
                "stylesheet @import tree exceeds the {MAX_STYLESHEET_FILES} file / \
                 {MAX_STYLESHEET_BYTES} byte limit"
            )));
        }
        budget.files = files;
        budget.bytes = next_bytes;
        Ok(ReadReservation { bytes })
    }

    /// Reconcile a metadata reservation with the exact bytes returned by the read.
    fn reconcile(
        &self,
        reservation: ReadReservation,
        actual_bytes: usize,
    ) -> Result<(), std::io::Error> {
        let mut budget = self
            .budget
            .lock()
            .expect("css read budget should not be poisoned");
        let without_reservation = budget
            .bytes
            .checked_sub(reservation.bytes)
            .expect("CSS read bytes must have an existing reservation");
        let reconciled_bytes = without_reservation.saturating_add(actual_bytes);
        if reconciled_bytes > MAX_STYLESHEET_BYTES {
            return Err(std::io::Error::other(format!(
                "stylesheet @import tree exceeds the {MAX_STYLESHEET_FILES} file / \
                 {MAX_STYLESHEET_BYTES} byte limit"
            )));
        }
        budget.bytes = reconciled_bytes;
        Ok(())
    }

    /// The set of real files Lightning CSS read — the entries plus every resolved `@import` child.
    /// Consumed after bundling is done (the provider must outlive the `StyleSheet`).
    fn into_read_inputs(self) -> CssReadInputs {
        let mut paths: Vec<PathBuf> = self
            .read_paths
            .into_inner()
            .expect("css read-path set should not be poisoned")
            .into_iter()
            .collect();
        paths.sort();
        let mut fingerprints = self
            .read_time_fingerprints
            .into_inner()
            .expect("CSS read-time fingerprint list should not be poisoned");
        sort_and_dedup_fingerprints(&mut fingerprints);
        let mut failed_paths = self
            .failed_paths
            .into_inner()
            .expect("CSS failed-path set should not be poisoned")
            .into_iter()
            .collect::<Vec<_>>();
        failed_paths.sort_by(|left, right| left.path.cmp(&right.path));
        failed_paths.dedup();
        CssReadInputs {
            paths,
            fingerprints,
            failed_paths,
        }
    }

    fn record_read(&self, path: PathBuf, fingerprint: Option<FileFingerprint>) {
        self.read_paths
            .lock()
            .expect("CSS read-path set should not be poisoned")
            .insert(path);
        if let Some(fingerprint) = fingerprint {
            self.read_time_fingerprints
                .lock()
                .expect("CSS read-time fingerprint list should not be poisoned")
                .push(fingerprint);
        }
    }

    fn record_failed_read(&self, path: PathBuf, kind: std::io::ErrorKind) {
        self.failed_paths
            .lock()
            .expect("CSS failed-path set should not be poisoned")
            .push(FailedRead::new(path.clone(), kind));
        self.context
            .record_failed_path(&path, kind == std::io::ErrorKind::NotFound);
    }

    fn check_deadline(&self) -> Result<(), std::io::Error> {
        self.context.check_deadline()
    }

    fn read_referenced_asset(
        &self,
        path: &Path,
        kind: AssetKind,
    ) -> std::io::Result<CollectedAsset> {
        self.context.snapshot(path, kind)
    }

    fn should_continue_dependency_reads(&self) -> bool {
        self.context.check_deadline().is_ok()
    }
}

impl SourceProvider for TrackingProvider {
    type Error = std::io::Error;

    fn read<'a>(&'a self, file: &Path) -> Result<&'a str, Self::Error> {
        self.check_deadline()?;
        // The synthetic entry has no file behind it, so it is served from memory and never recorded
        // as a freshness input — there is nothing on disk that could change.
        if let Some((path, content)) = &self.synthetic
            && file == path
        {
            return Ok(content.as_str());
        }

        // Canonicalize so a cache key is stable across `..` / symlink spellings of the same file.
        let key = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        // Record the ATTEMPT before any fallible operation. A missing/broken child is part of why
        // processing fell back and must not disappear from freshness merely because it had no
        // bytes to hash.
        self.record_read(key.clone(), None);
        // This attempt's own entry first, then anything an earlier attempt already read. Reusing the
        // ledger's snapshot is what keeps a retry measuring the SAME bytes the union measured, and
        // what stops it from charging the same file twice.
        let snapshot = self
            .preloaded
            .get(&key)
            .cloned()
            .or_else(|| self.context.snapshot_for(&key));
        let source = match snapshot {
            Some(asset) => {
                self.context.charge_css_snapshot(&asset)?;
                self.reserve(asset.bytes().len())?;
                self.record_read(key.clone(), Some(asset.fingerprint.clone()));
                let bytes = self.retained_sources.retain_snapshot(asset.bytes_arc());
                std::str::from_utf8(bytes).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("stylesheet {} is not UTF-8: {error}", key.display()),
                    )
                })?
            }
            None => {
                // Stat BEFORE reading. If the file changes during the read, this pre-read metadata
                // can only cause a conservative hash check later; post-read metadata could match
                // the replacement and falsely bless old bytes forever.
                let metadata = match std::fs::metadata(&key) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        self.record_failed_read(key, error.kind());
                        return Err(error);
                    }
                };
                let shared_reservation = self.context.begin_css_read(&key, &metadata)?;
                let metadata_bytes = usize::try_from(metadata.len()).map_err(|_| {
                    std::io::Error::other(format!(
                        "stylesheet {} is too large for this platform",
                        key.display()
                    ))
                })?;
                let reservation = self.reserve(metadata_bytes)?;
                let bytes = match std::fs::read(&key) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.record_failed_read(key, error.kind());
                        return Err(error);
                    }
                };
                self.reconcile(reservation, bytes.len())?;
                let fingerprint = self
                    .context
                    .finish_css_read(shared_reservation, &bytes)?
                    .fingerprint;
                self.record_read(key.clone(), Some(fingerprint));
                let source = String::from_utf8(bytes).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("stylesheet {} is not UTF-8: {error}", key.display()),
                    )
                })?;
                self.retained_sources.retain(source)
            }
        };
        self.check_deadline()?;
        Ok(source)
    }

    fn resolve(
        &self,
        specifier: &str,
        originating_file: &Path,
    ) -> Result<ResolveResult, Self::Error> {
        // A REMOTE `@import` (`@import url("https://fonts.googleapis.com/…")`) has no file behind it
        // and is not ours to inline — a real bundler leaves it in the sheet as an import, and so do
        // we. Reporting it as external keeps the rest of the stylesheet counted; treating it as a
        // resolve failure would sink the whole set to raw disclosure over a shape ordinary packages
        // ship.
        if is_remote_specifier(specifier) {
            return Ok(ResolveResult::External(specifier.to_owned()));
        }

        // The synthetic entry `@import`s absolute paths; resolve those directly. `FileProvider`'s
        // own resolve is a naive relative join and would mangle them.
        let candidate = Path::new(specifier);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            match self.inner.resolve(specifier, originating_file)? {
                ResolveResult::File(path) => path,
                external @ ResolveResult::External(_) => return Ok(external),
            }
        };

        // CANONICALIZE, and not for tidiness: Lightning CSS cycle-detects on the very PathBuf
        // spelling this returns, and `FileProvider::resolve` is a naive `with_file_name` join that
        // never normalizes `..`. A cycle that crosses a `../` therefore hands back a LONGER, DISTINCT
        // key for the SAME file on every hop, the dedup never fires, and the recursion overflows the
        // stack — which `catch_unwind` cannot catch, so the daemon dies outright rather than failing
        // one import. `node_modules` is untrusted input and an `@import` cycle is silent in browsers
        // and in every real bundler (they dedupe on a resolved URL), so a package can ship one and
        // never know. Canonicalizing makes the key an identity, and the cycle terminates.
        Ok(ResolveResult::File(
            std::fs::canonicalize(&resolved).unwrap_or(resolved),
        ))
    }
}

/// An `@import` that names a network resource rather than a file on disk.
fn is_remote_specifier(specifier: &str) -> bool {
    let lower = specifier.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//")
}

/// A reachable stylesheet set processed as it ships: the bundled bytes before and after
/// minification, plus every file that fed them (for freshness).
#[derive(Debug)]
pub struct CssBundle {
    /// The `@import`-inlined stylesheet before minification, mirroring the JS chunk's `raw_bytes`.
    pub raw_bytes: Vec<u8>,
    /// The `@import`-inlined, minified stylesheet: what actually ships, and what gets compressed.
    pub minified_bytes: Vec<u8>,
    pub read_paths: Vec<PathBuf>,
    pub read_time_fingerprints: Vec<FileFingerprint>,
    failed_paths: Vec<FailedRead>,
    /// Supported local artifacts referenced by the CSS that survives minification. They are
    /// separate emitted files, so the caller processes and compresses them independently.
    pub referenced_assets: Vec<CollectedAsset>,
    /// Local supported resources that survived CSS minification but could not be read.
    pub(crate) referenced_failures: Vec<crate::pipeline::css_dependencies::CssDependencyFailure>,
    /// Local resources the stylesheet ships that are outside the counted taxonomy — images, SVG.
    /// Disclosed at their real size, never counted, and never silently dropped.
    pub referenced_uncounted: Vec<UncountedAsset>,
    /// Local resources that ship but whose bytes could not even be sized, including the case where
    /// dependency analysis could not inspect a sheet's URLs at all (an ambiguous relative URL in a
    /// custom property fails the metadata-only print while both measuring prints succeed).
    ///
    /// Dependency analysis is metadata-only and must never discard an otherwise valid CSS size, so
    /// the CSS contribution is kept and the omission is disclosed. It is an OMISSION, not an
    /// over-count: these bytes are missing from the total, so they make the result a floor.
    pub dependency_omissions: Vec<String>,
    /// Runtime-fetched resources: real weight, but not bytes this package ships, so the measured
    /// size is exact and stays budgetable.
    pub dependency_external: Vec<String>,
}

/// A read that failed, and WHY — because the two reasons must not be treated alike.
///
/// A file that simply is not there is a deterministic fact about the package: it will keep not being
/// there until someone creates it, and creating it invalidates the result through the never-fresh
/// sentinel below. A permission error, a lock, or a file deleted mid-build is a fact about this
/// machine at this moment.
///
/// Both must stay never-fresh. Only the second may contribute the request-local `asset_io` stage —
/// conflating them meant one missing `@import` target refused the WHOLE asset result from every
/// durable store, so the package was re-measured on every keystroke over a file nobody was going to
/// create. One collection rather than two parallel ones: two vectors that must agree is the shape
/// that drifts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FailedRead {
    path: PathBuf,
    missing: bool,
}

impl FailedRead {
    fn new(path: PathBuf, kind: std::io::ErrorKind) -> Self {
        Self {
            missing: kind == std::io::ErrorKind::NotFound,
            path,
        }
    }
}

#[derive(Debug, Default)]
struct CssReadInputs {
    paths: Vec<PathBuf>,
    fingerprints: Vec<FileFingerprint>,
    failed_paths: Vec<FailedRead>,
}

impl CssReadInputs {
    fn extend(&mut self, other: Self) {
        self.paths.extend(other.paths);
        self.fingerprints.extend(other.fingerprints);
        self.failed_paths.extend(other.failed_paths);
    }
}

#[derive(Debug)]
struct CssProcessingError {
    message: String,
    inputs: CssReadInputs,
    non_durable_stages: BTreeSet<&'static str>,
}

impl CssProcessingError {
    fn from_transform(message: String, inputs: CssReadInputs) -> Self {
        let mut non_durable_stages = BTreeSet::new();
        if inputs.failed_paths.iter().any(|failed| !failed.missing) {
            non_durable_stages.insert(crate::engine::stage::ASSET_IO);
        }
        Self {
            message,
            inputs,
            non_durable_stages,
        }
    }

    fn from_compression(message: String, inputs: CssReadInputs) -> Self {
        let mut error = Self::from_transform(message, inputs);
        error
            .non_durable_stages
            .insert(crate::pipeline::stage::COMPRESSION);
        error
    }
}

/// Bundle one stylesheet the way it ships: resolve its `@import` tree from disk into one
/// stylesheet, minify with deterministic (target-free) output, and print. Returns the bytes and the
/// set of files read. Any failure is an `Err`; the caller falls back to raw-byte disclosure so the
/// result never drops below today's behavior.
///
/// Test-only, and it builds a REAL ledger rather than skipping one. There used to be an unbounded
/// path here so processor tests could avoid constructing a context, which meant the tests measured
/// through code production never runs — the one place where "it passed in tests" is worth least.
#[cfg(test)]
pub fn bundle_css(entry: &Path) -> Result<CssBundle, String> {
    let asset = read_collected_asset(entry, AssetKind::Css)
        .map_err(|error| format!("failed to read stylesheet {}: {error}", entry.display()))?;
    let context = test_context(std::slice::from_ref(&asset));
    bundle_collected_css(&asset, context).map_err(|error| error.message)
}

/// A production-shaped ledger for a test that only wants to bundle something.
///
/// Production limits deliberately: a test that quietly ran under looser bounds than the daemon
/// would be measuring a different system. The per-attempt stylesheet-tree bound lives on the
/// provider and still applies, which is what the budget tests exercise.
#[cfg(test)]
fn process_assets_for_test(assets: &[CollectedAsset]) -> ProcessedAssets {
    process_assets(assets, test_context(assets))
        .expect("a generous test ledger cannot hit a shared build limit")
}

#[cfg(test)]
fn test_context(entries: &[CollectedAsset]) -> Arc<AssetProcessingContext> {
    test_context_with(entries, AssetBudgetLimits::production())
}

#[cfg(test)]
fn test_context_with(
    entries: &[CollectedAsset],
    limits: AssetBudgetLimits,
) -> Arc<AssetProcessingContext> {
    Arc::new(AssetProcessingContext::new(
        0,
        &[],
        entries,
        crate::pipeline::asset_boundary::AssetDeadline::for_test(std::time::Duration::from_secs(
            30,
        )),
        limits,
    ))
}

fn bundle_collected_css(
    entry: &CollectedAsset,
    context: Arc<AssetProcessingContext>,
) -> Result<CssBundle, CssProcessingError> {
    let provider = TrackingProvider::new(std::slice::from_ref(entry), None, context);
    let result = bundle_with(&provider, &entry.path);
    let inputs = provider.into_read_inputs();
    match result {
        Ok(mut bundle) => {
            bundle.read_paths = inputs.paths;
            bundle.read_time_fingerprints = inputs.fingerprints;
            bundle.failed_paths = inputs.failed_paths;
            Ok(bundle)
        }
        Err(message) => Err(CssProcessingError::from_transform(message, inputs)),
    }
}

/// Bundle EVERY reachable stylesheet into one artifact, which is how CSS ships and how the esbuild
/// oracle emits it. A single entry bundles directly; several are combined behind a synthetic entry
/// that `@import`s each, so Lightning CSS inlines and dedupes them into one sheet rather than us
/// summing overlapping copies.
#[cfg(test)]
pub fn bundle_css_set(entries: &[PathBuf]) -> Result<CssBundle, String> {
    let entries = entries
        .iter()
        .map(|entry| {
            read_collected_asset(entry, AssetKind::Css)
                .map_err(|error| format!("failed to read stylesheet {}: {error}", entry.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let context = test_context(&entries);
    bundle_collected_css_set(&entries, context).map_err(|error| error.message)
}

fn bundle_collected_css_set(
    entries: &[CollectedAsset],
    context: Arc<AssetProcessingContext>,
) -> Result<CssBundle, CssProcessingError> {
    match entries {
        [] => Err(CssProcessingError {
            message: "no stylesheets to bundle".to_owned(),
            inputs: CssReadInputs::default(),
            non_durable_stages: BTreeSet::new(),
        }),
        [single] => bundle_collected_css(single, context),
        many => {
            let paths = many
                .iter()
                .map(|asset| asset.path.clone())
                .collect::<Vec<_>>();
            let (path, content) = synthetic_entry(&paths);
            let provider = TrackingProvider::new(many, Some((path.clone(), content)), context);
            let result = bundle_with(&provider, &path);
            let inputs = provider.into_read_inputs();
            match result {
                Ok(mut bundle) => {
                    bundle.read_paths = inputs.paths;
                    bundle.read_time_fingerprints = inputs.fingerprints;
                    bundle.failed_paths = inputs.failed_paths;
                    Ok(bundle)
                }
                Err(message) => Err(CssProcessingError::from_transform(message, inputs)),
            }
        }
    }
}

/// The virtual entry that unions several stylesheets, placed in a real directory so anything
/// resolved relative to it still lands somewhere sane, under a name no package would ship.
fn synthetic_entry(entries: &[PathBuf]) -> (PathBuf, String) {
    let directory = entries[0]
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let path = directory.join("__import_lens_combined_stylesheets__.css");

    let content = entries
        .iter()
        .map(|entry| {
            format!(
                "@import \"{}\";",
                css_string_escape(&entry.to_string_lossy())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    (path, content)
}

/// Escape a path for use inside a CSS string.
///
/// Backslash and double-quote are the only characters that can end the string or start an escape, so
/// escaping them is the whole job — and it is not theoretical: a package is free to ship a file whose
/// name contains a quote (POSIX allows it), which would otherwise close the string and inject rules
/// into the sheet we are about to measure. `node_modules` is untrusted input.
///
/// This replaces a blanket `\` -> `/` rewrite, which was wrong twice over: it corrupted a legitimate
/// POSIX path containing a literal backslash, and on Windows it turned a verbatim `\\?\C:\…` prefix
/// into `//?/C:/…`, which is NOT verbatim — silently switching off the `..` normalization that
/// `PathBuf` applies to verbatim paths, and with it the only thing keeping an `@import` cycle from
/// recursing forever. Backslashes are kept and escaped instead.
fn css_string_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The provider must outlive the `StyleSheet` (it borrows source strings held inside it), so all
/// consumption happens here and only owned bytes escape.
fn bundle_with(provider: &TrackingProvider, entry: &Path) -> Result<CssBundle, String> {
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;
    let mut bundler = Bundler::new(provider, None, ParserOptions::default());
    let mut stylesheet = bundler.bundle(entry).map_err(|error| {
        format!(
            "lightningcss failed to bundle {}: {error:?}",
            entry.display()
        )
    })?;
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;

    // Print before minifying: this is the bundled sheet as authored, the CSS counterpart of the JS
    // chunk's unminified `raw_bytes`.
    let raw_bytes = stylesheet
        .to_css(PrinterOptions {
            minify: false,
            targets: Targets::default(),
            ..Default::default()
        })
        .map_err(|error| {
            format!(
                "lightningcss failed to print {}: {error:?}",
                entry.display()
            )
        })?
        .code
        .into_bytes();
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;

    stylesheet
        .minify(MinifyOptions {
            targets: Targets::default(),
            ..Default::default()
        })
        .map_err(|error| {
            format!(
                "lightningcss failed to minify {}: {error:?}",
                entry.display()
            )
        })?;
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;

    // This is a metadata-only print. Lightning CSS replaces every URL in its returned code with a
    // hashed placeholder when dependency analysis is enabled, so that code must never become the
    // measured artifact. `to_css` borrows immutably; the ordinary print below still emits the real
    // minified stylesheet. Analyze after minification so a resource removed from shipped CSS is not
    // counted.
    let (
        referenced_assets,
        referenced_failures,
        referenced_uncounted,
        dependency_omissions,
        dependency_external,
    ) = stylesheet
        .to_css(PrinterOptions {
            minify: true,
            targets: Targets::default(),
            analyze_dependencies: Some(DependencyOptions::default()),
            ..Default::default()
        })
        .map(|result| {
            let dependencies = collect_referenced_assets(
                result.dependencies.unwrap_or_default(),
                &|path, kind| provider.read_referenced_asset(path, kind),
                &|| provider.should_continue_dependency_reads(),
            );
            (
                dependencies.assets,
                dependencies.failures,
                dependencies.uncounted,
                dependencies.omissions,
                dependencies.external,
            )
        })
        .unwrap_or_else(|error| {
            // This print is the ONLY one with dependency analysis enabled, and lightningcss has
            // errors that fire only in that mode (an ambiguous relative `url()` in a custom
            // property). So the sheet measures fine while its whole `url()` graph goes
            // undiscovered — an omission of unknown size, disclosed as one.
            (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![format!(
                    "lightningcss could not inspect resource URLs in {}: {error:?}",
                    entry.display()
                )],
                Vec::new(),
            )
        });
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;

    let minified_bytes = stylesheet
        .to_css(PrinterOptions {
            minify: true,
            targets: Targets::default(),
            ..Default::default()
        })
        .map_err(|error| {
            format!(
                "lightningcss failed to print {}: {error:?}",
                entry.display()
            )
        })?
        .code
        .into_bytes();
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;

    Ok(CssBundle {
        raw_bytes,
        minified_bytes,
        read_paths: Vec::new(),
        read_time_fingerprints: Vec::new(),
        failed_paths: Vec::new(),
        referenced_assets,
        referenced_failures,
        referenced_uncounted,
        dependency_omissions,
        dependency_external,
    })
}

/// What the reachable assets really cost, ready to fold into the Import Cost.
#[derive(Debug, Default)]
pub struct ProcessedAssets {
    /// One entry per asset kind actually present, already summed across that kind's artifacts.
    pub contributions: Vec<AssetContribution>,
    /// Files the processing discovered outside the JavaScript graph — a stylesheet's `@import`
    /// children and supported local `url()` artifacts. Without these in freshness, editing one
    /// would not invalidate the size it fed.
    pub read_paths: Vec<PathBuf>,
    /// Fingerprints captured by the same reads that supplied every measured asset byte.
    pub read_time_fingerprints: Vec<FileFingerprint>,
    /// Paths whose read failed during any attempt. A later success in the same union/retry flow
    /// does not erase the failure: that mixed observation cannot produce a reusable cache entry.
    failed_paths: Vec<FailedRead>,
    /// Assets that could NOT be processed, disclosed with their raw bytes exactly as before.
    pub uncounted: Vec<UncountedAsset>,
    /// Why each of those fell back, for the diagnostic.
    pub failures: Vec<String>,
    /// Why the stylesheet set could not be bundled as ONE artifact, when it could not.
    ///
    /// Every sheet is still counted, so this leaves `uncounted` EMPTY — which is exactly why it is
    /// its own field instead of a line in `failures`. `failures` is read only as the detail of the
    /// `uncounted` disclosure, so a degradation with no uncounted asset had nothing to hang from
    /// and was dropped on the floor, taking a silent, High-confidence, cacheable over-count with
    /// it. Every channel here now has one consumer and its own trigger.
    pub stylesheets_measured_separately: Option<String>,
    /// Local resources a counted stylesheet references that are missing from the total and whose
    /// size is not even known — an unlocatable path, an unreadable file, or a sheet whose URLs
    /// could not be inspected at all.
    ///
    /// This is an OMISSION channel. It used to be disclosed as `imprecise_assets`, the stage that
    /// means the number reads HIGH, which pointed the disclosure in the opposite direction from the
    /// error: `incomplete` never fired, so a total missing real shipped bytes was cached and
    /// recorded as a file's permanent baseline.
    pub css_dependency_omissions: Vec<String>,
    /// Runtime-fetched resources a counted stylesheet references. Disclosed, but the measured bytes
    /// are exact without them, so this must not touch completeness or budgetability.
    pub css_dependency_external: Vec<String>,
    /// Machine/request-local causes retained structurally so neither cache admission nor wire
    /// consumers have to infer durability from human-readable error text.
    non_durable_stages: BTreeSet<&'static str>,
}

impl ProcessedAssets {
    /// The five sizes of every counted asset, summed. Each artifact was already compressed on its
    /// own, so this only adds their numbers to the JavaScript chunk's.
    pub fn total(&self) -> MeasuredSizes {
        let mut total = MeasuredSizes::ZERO;
        for contribution in &self.contributions {
            total.raw_bytes += contribution.raw_bytes;
            total.minified_bytes += contribution.minified_bytes;
            total.gzip_bytes += contribution.gzip_bytes;
            total.brotli_bytes += contribution.brotli_bytes;
            total.zstd_bytes += contribution.zstd_bytes;
        }
        total
    }

    pub fn is_empty(&self) -> bool {
        self.contributions.is_empty()
    }

    /// Whether supported asset bytes are disclosed but absent from [`Self::total`]. A deterministic
    /// processor rejection is reusable at the import level, but any aggregate containing it is a
    /// lower bound rather than a complete File Cost.
    pub fn has_uncounted_assets(&self) -> bool {
        // A CSS-referenced omission counts here even though it has no `UncountedAsset` row: the
        // bytes are missing from the total just the same, and the fact that their size is unknown
        // makes the result MORE of a floor, not less. Reading only `uncounted` is what let an
        // omission through as a complete measurement.
        !self.uncounted.is_empty() || !self.css_dependency_omissions.is_empty()
    }

    /// Exact asset fingerprints plus never-fresh sentinels for attempted paths that supplied no
    /// bytes. Both Import Cost and File Cost consume this one normalization so neither can silently
    /// drop an unreadable CSS child or resource from freshness.
    pub fn freshness_fingerprints(&self) -> Vec<FileFingerprint> {
        let mut fingerprints = self.read_time_fingerprints.clone();
        let fingerprinted = fingerprints
            .iter()
            .map(|fingerprint| fingerprint.path.clone())
            .collect::<HashSet<_>>();
        // A path that was MISSING gets the absent sentinel: fresh while it stays missing, stale
        // the moment it appears. A path that failed for any other reason stays unverifiable, which
        // is never fresh, because we cannot say what state it was in.
        let mut absent_paths = self
            .failed_paths
            .iter()
            .filter(|failed| failed.missing)
            .map(|failed| failed.path.clone())
            .collect::<Vec<_>>();
        absent_paths.sort();
        absent_paths.dedup();
        let missing = absent_paths.iter().cloned().collect::<HashSet<_>>();
        fingerprints.extend(
            absent_paths
                .into_iter()
                .map(crate::cache::key::absent_file_fingerprint),
        );
        let mut failed_paths = self
            .failed_paths
            .iter()
            .filter(|failed| !failed.missing)
            .map(|failed| failed.path.clone())
            .collect::<Vec<_>>();
        // Defensive inference for any future processor that records an attempted path but forgets
        // to classify the failed read explicitly. Explicit failures remain even if another attempt
        // later fingerprinted the same path.
        // A path already recorded as absent is excluded: giving it the unverifiable sentinel too
        // would put two different sentinels on one path, which reads as a conflicting observation
        // and refuses the whole result — the exact reuse this fix exists to restore.
        failed_paths.extend(
            self.read_paths
                .iter()
                .filter(|path| !fingerprinted.contains(&path.to_string_lossy().replace('\\', "/")))
                .filter(|path| !missing.contains(*path))
                .cloned(),
        );
        failed_paths.sort();
        failed_paths.dedup();
        fingerprints.extend(
            failed_paths
                .into_iter()
                .map(crate::cache::key::unverifiable_file_fingerprint),
        );
        sort_and_dedup_fingerprints(&mut fingerprints);
        fingerprints
    }
}

/// The disclosure for assets that could NOT be processed. This is the pre-B2 behaviour kept as the
/// fallback: their bytes are real, they ship, and they are not in the number — which is exactly
/// what this stage has always meant. `None` when everything was counted, which is the normal case,
/// and that absence is what lets a CSS-shipping package leave Medium confidence.
pub fn uncounted_assets_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    if processed.uncounted.is_empty() {
        return None;
    }

    Some(ImportDiagnostic {
        stage: diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
        message: crate::engine::uncounted_assets_message(&processed.uncounted),
        details: processed.failures.clone(),
    })
}

/// The disclosure for assets that ARE counted but whose bytes are counted more than once.
///
/// The union buys TWO things, and losing it costs both: it dedupes an `@import` two sheets share,
/// AND it puts the whole set through ONE compression stream. When it fails, each sheet is measured
/// and compressed alone, so shared bytes are inlined into each and no sheet's compressor can reach
/// what the others contain. The second term dominates, by a lot: 300 tiny sheets sharing no
/// `@import` at all — the shape that actually breaches the file budget — sum to ~40x the union's
/// gzip and ~57x its brotli, because every stream restarts its window and pays its own header.
///
/// That is why this fires on the union having failed, not on the sheets provably sharing bytes.
/// Sheets that share nothing are not the safe case to stay quiet about; they are the worst one.
/// `None` in the normal case, where the union held.
///
/// This is separate from [`uncounted_assets_diagnostic`] because it reports a different fact: bytes
/// present but over-counted, not bytes missing. Folding it into that one is what hid it — that
/// function returns early when nothing is uncounted, which is exactly the degraded case.
pub fn imprecise_assets_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    let reason = processed.stylesheets_measured_separately.as_ref()?;

    Some(ImportDiagnostic {
        stage: diagnostic_stage::IMPRECISE_ASSETS.to_owned(),
        message: "the stylesheets could not be bundled as one artifact, so each was measured and \
                  compressed on its own: bytes two sheets share are counted once per sheet, and \
                  no sheet's compression can use what the others contain, so this size reads HIGH"
            .to_owned(),
        details: vec![reason.clone()],
    })
}

/// The disclosure for local resources a counted stylesheet references but the size does not include.
///
/// `UNCOUNTED_ASSETS`, not `IMPRECISE_ASSETS`. Those two stages point in opposite directions:
/// imprecise means bytes are counted more than once and the number reads HIGH, uncounted means
/// bytes are missing and the number is a floor. This channel is the second one, and reporting it as
/// the first is what stopped `incomplete` from firing — so a File Cost short by real shipped bytes
/// passed every durability gate and was written to the no-TTL history as that file's baseline.
fn omitted_css_resources_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    if processed.css_dependency_omissions.is_empty() {
        return None;
    }

    Some(ImportDiagnostic {
        stage: diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
        message: "the stylesheet was measured, but local resources it references could not be \
                  located or inspected, so this size does NOT include their shipped bytes"
            .to_owned(),
        details: processed.css_dependency_omissions.clone(),
    })
}

/// The disclosure for resources a counted stylesheet fetches at runtime rather than ships.
///
/// `EXTERNAL`, which is durable AND budgetable, because the measured bytes are exact without them:
/// a CDN font is weight the page pays but not weight this package carries, so refusing to judge the
/// number would be wrong. Routing these through a precision stage is what silently disabled budget
/// verdicts for every package that `@import`s a web font. The disclosure still costs High
/// confidence, which is the honest reading — there is runtime weight this number does not model.
fn external_css_resources_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    if processed.css_dependency_external.is_empty() {
        return None;
    }

    Some(ImportDiagnostic {
        stage: diagnostic_stage::EXTERNAL.to_owned(),
        message:
            "the stylesheet references resources fetched at runtime; they are real weight for \
                  the page but are not bytes this package ships, so this size excludes them"
                .to_owned(),
        details: processed.css_dependency_external.clone(),
    })
}

fn asset_io_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    let mut paths = processed
        .failed_paths
        .iter()
        .filter(|failed| !failed.missing)
        .map(|failed| failed.path.clone())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    paths.sort();
    paths.dedup();
    Some(ImportDiagnostic {
        stage: crate::engine::stage::ASSET_IO.to_owned(),
        message: "one or more asset inputs could not be read during this analysis; the result \
                  reflects a changing or unavailable filesystem and will not be reused"
            .to_owned(),
        details: paths
            .iter()
            .map(|path| format!("unreadable asset input: {}", path.display()))
            .collect(),
    })
}

fn asset_compression_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    processed
        .non_durable_stages
        .contains(crate::pipeline::stage::COMPRESSION)
        .then(|| ImportDiagnostic {
            stage: crate::pipeline::stage::COMPRESSION.to_owned(),
            message: "an asset compressor failed during this analysis; the partial asset size is \
                      request-local and will not be reused"
                .to_owned(),
            details: Vec::new(),
        })
}

/// Every disclosure the processed assets owe the user, so a caller cannot fold in the bytes and
/// forget one. Both call sites take the whole list rather than naming the diagnostics one by one:
/// a future asset caveat is then disclosed by construction instead of by memory.
pub fn asset_diagnostics(processed: &ProcessedAssets) -> Vec<ImportDiagnostic> {
    [
        uncounted_assets_diagnostic(processed),
        imprecise_assets_diagnostic(processed),
        omitted_css_resources_diagnostic(processed),
        external_css_resources_diagnostic(processed),
        asset_io_diagnostic(processed),
        asset_compression_diagnostic(processed),
    ]
    .into_iter()
    .flatten()
    .collect()
}

const ASSET_PROCESSING_TIMEOUT: Duration = Duration::from_secs(8);

/// A whole post-build asset stage failed before it could produce one coherent measurement.
#[derive(Debug, Clone)]
pub struct AssetProcessingFailure {
    pub(crate) stage: &'static str,
    pub(crate) message: String,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) read_time_fingerprints: Vec<FileFingerprint>,
}

impl From<AssetBudgetFailure> for AssetProcessingFailure {
    fn from(failure: AssetBudgetFailure) -> Self {
        Self {
            stage: failure.stage.as_str(),
            message: failure.message,
            read_paths: failure.read_paths,
            read_time_fingerprints: failure.read_time_fingerprints,
        }
    }
}

fn boundary_failure(error: AssetBoundaryError) -> AssetProcessingFailure {
    let stage = match error {
        AssetBoundaryError::AdmissionTimedOut { .. }
        | AssetBoundaryError::ExecutionTimedOut { .. } => crate::engine::stage::TIMEOUT,
        AssetBoundaryError::Panicked { .. } => crate::engine::stage::PANIC,
        AssetBoundaryError::AdmissionFailed { .. } => crate::engine::stage::ENGINE_GONE,
    };
    AssetProcessingFailure {
        stage,
        message: error.to_string(),
        read_paths: Vec::new(),
        read_time_fingerprints: Vec::new(),
    }
}

/// Production entry: asset work has its own two-wide admission gate and one absolute deadline.
///
/// Public because the freshness integration test measures through it. There used to be an unbounded
/// entry point for that, which meant the test measured code production never runs.
pub fn process_assets_bounded(
    assets: Vec<CollectedAsset>,
    graph_source_bytes: usize,
    graph_loaded_paths: Vec<PathBuf>,
) -> Result<ProcessedAssets, AssetProcessingFailure> {
    if assets.is_empty() {
        return Ok(ProcessedAssets::default());
    }
    asset_boundary::execute(ASSET_PROCESSING_TIMEOUT, move |deadline: AssetDeadline| {
        let context = Arc::new(AssetProcessingContext::production(
            graph_source_bytes,
            &graph_loaded_paths,
            &assets,
            deadline,
        ));
        process_assets(&assets, context).map_err(AssetProcessingFailure::from)
    })
    .map_err(boundary_failure)?
}

/// Process every reachable asset the build collected, the way each really ships.
///
/// Never fails on an asset it cannot process: that falls back to the raw-byte disclosure that was
/// the whole behaviour before B2, so the result is a strict improvement or a tie. It DOES fail when
/// the shared build ledger is exhausted, which is a fact about the build rather than about any one
/// asset.
fn process_assets(
    assets: &[CollectedAsset],
    context: Arc<AssetProcessingContext>,
) -> Result<ProcessedAssets, AssetBudgetFailure> {
    let mut processed = ProcessedAssets::default();
    if let Some(failure) = context.failure() {
        return Err(failure);
    }
    if assets.is_empty() {
        return Ok(processed);
    }

    let referenced_assets = process_stylesheets(assets, &mut processed, context.clone())?;
    let mut assets_by_path: BTreeMap<PathBuf, CollectedAsset> = assets
        .iter()
        .cloned()
        .map(|asset| (asset.path.clone(), asset))
        .collect();
    for asset in referenced_assets {
        processed.read_paths.push(asset.path.clone());
        // Count one emitted file per path, but retain every observation. If the same resource
        // changed between a direct graph load and CSS dependency analysis (or between per-sheet
        // retries), the conflicting fingerprints make this run non-reusable instead of silently
        // blessing whichever snapshot won the byte deduplication.
        processed
            .read_time_fingerprints
            .push(asset.fingerprint.clone());
        assets_by_path.entry(asset.path.clone()).or_insert(asset);
    }
    let all_assets: Vec<CollectedAsset> = assets_by_path.into_values().collect();

    processed
        .read_paths
        .extend(all_assets.iter().map(|asset| asset.path.clone()));
    processed
        .read_time_fingerprints
        .extend(all_assets.iter().map(|asset| asset.fingerprint.clone()));

    for kind in [AssetKind::Wasm, AssetKind::Font] {
        process_binary_kind(&all_assets, kind, &mut processed, &context)?;
    }

    // The shared ledger also observes metadata reservations that fail before a provider read and
    // exact snapshots served across retry providers. Merge that whole history on success so a
    // later successful retry cannot erase an earlier conflicting/failed observation from cache
    // freshness merely because both used the same path.
    processed.read_paths.extend(context.read_paths());
    processed
        .read_time_fingerprints
        .extend(context.freshness_fingerprints());

    processed
        .contributions
        .sort_by_key(|contribution| contribution.kind);
    processed.read_paths.sort();
    processed.read_paths.dedup();
    sort_and_dedup_fingerprints(&mut processed.read_time_fingerprints);
    if let Some(failure) = context.failure() {
        return Err(failure);
    }
    Ok(processed)
}

/// The settled result of bundling the stylesheet set, named rather than a bare tuple so that the
/// degraded flag cannot be dropped on its way out of the retry.
struct StylesheetOutcome {
    counted: Vec<(CssBundle, CompressionSizes)>,
    /// Why individual sheets fell back, one per entry in `uncounted`.
    failures: Vec<String>,
    uncounted: Vec<UncountedAsset>,
    /// Inputs observed by failed union/per-sheet attempts. Successful bundles carry their own.
    observed_inputs: CssReadInputs,
    /// A request-local cause stays sticky across the union/per-sheet retry. A later success must
    /// not turn an earlier filesystem/compressor failure into a reusable package fact.
    non_durable_stages: BTreeSet<&'static str>,
    /// `Some(union error)` when the set was measured one sheet at a time.
    degraded: Option<String>,
}

fn may_retry_stylesheets_separately(error: &CssProcessingError) -> bool {
    !error
        .non_durable_stages
        .contains(crate::pipeline::stage::COMPRESSION)
}

fn process_stylesheets(
    assets: &[CollectedAsset],
    processed: &mut ProcessedAssets,
    context: Arc<AssetProcessingContext>,
) -> Result<Vec<CollectedAsset>, AssetBudgetFailure> {
    let entries: Vec<CollectedAsset> = assets
        .iter()
        .filter(|asset| asset.kind == AssetKind::Css)
        .cloned()
        .collect();
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // One artifact for the whole set is the right answer (it is how CSS ships, and it dedupes what
    // two sheets share) — but it is all-or-nothing, and a set fails as a unit. A package that ships
    // one `.scss`, or one sheet with a bare `@import` Lightning CSS cannot resolve, would take every
    // other stylesheet down with it, and in the File Cost's combined build that set spans every
    // import in the runtime group. So if the union fails, retry per sheet: the ones that parse are
    // still counted and only the offender falls back. Sheets that share an `@import` are no longer
    // deduped in that degraded mode, which is a smaller and rarer error than dropping them all.
    // NOTHING is written to `processed` until the outcome is settled. The retry used to report each
    // sheet's failure as it went and then hand back an `Err` when none survived, so the all-fail path
    // disclosed every stylesheet TWICE and the diagnostic doubled its own count and byte total. The
    // per-sheet results are gathered locally and committed once, which removes that shape rather than
    // patching it.
    //
    // The degradation is recorded as its own state, NOT as a line in `failures`. `failures` is read
    // only as the detail of the uncounted disclosure, which goes silent when every sheet counts —
    // the common outcome here, since the union's usual reason to fail is the whole set breaching a
    // budget that each sheet is well inside. That silence made this path report an over-count at
    // High confidence and cache it.
    let bundled = bundle_collected_css_set(&entries, context.clone())
        .and_then(|bundle| compress_bundle(bundle, &context))
        .map(|counted| StylesheetOutcome {
            counted: vec![counted],
            failures: Vec::new(),
            uncounted: Vec::new(),
            observed_inputs: CssReadInputs::default(),
            non_durable_stages: BTreeSet::new(),
            degraded: None,
        })
        .or_else(|union_error| {
            // An overall resource/deadline failure is final. Retrying each sheet would reset only
            // the local tree counter and repeat work after the build-wide ledger was exhausted.
            if context.failure().is_some() {
                return Err(union_error);
            }
            // Compression says nothing about whether the CSS union is structurally invalid.
            // Splitting a valid artifact after a compressor failure changes the quantity and can
            // produce a separately-compressed over-count, so fall back with the typed cause.
            if !may_retry_stylesheets_separately(&union_error) {
                return Err(union_error);
            }
            if entries.len() == 1 {
                return Err(union_error);
            }

            let CssProcessingError {
                message: union_message,
                inputs: mut observed_inputs,
                mut non_durable_stages,
            } = union_error;
            let mut counted = Vec::new();
            let mut failures = Vec::new();
            let mut uncounted = Vec::new();

            for entry in &entries {
                if context.failure().is_some() {
                    break;
                }
                match bundle_collected_css(entry, context.clone())
                    .and_then(|bundle| compress_bundle(bundle, &context))
                {
                    Ok(bundled) => counted.push(bundled),
                    Err(error) => {
                        failures.push(error.message);
                        observed_inputs.extend(error.inputs);
                        non_durable_stages.extend(error.non_durable_stages);
                        uncounted.push(UncountedAsset {
                            path: entry.path.clone(),
                            bytes: entry.raw_bytes(),
                        });
                    }
                }
            }

            // Every sheet failed, so this is simply the pre-B2 fallback: hand back the union's error
            // and let the one disclosure below cover them, exactly once each.
            if counted.is_empty() {
                return Err(CssProcessingError {
                    message: union_message,
                    inputs: observed_inputs,
                    non_durable_stages,
                });
            }
            Ok(StylesheetOutcome {
                counted,
                failures,
                uncounted,
                observed_inputs,
                non_durable_stages,
                // Sheets DID count here, so `uncounted` may well be empty and the uncounted
                // disclosure silent. This is what makes the over-count speakable.
                degraded: Some(union_message),
            })
        });

    if let Some(failure) = context.failure() {
        return Err(failure);
    }

    let mut referenced_assets = Vec::new();
    match bundled {
        Ok(StylesheetOutcome {
            counted,
            failures,
            uncounted,
            observed_inputs,
            non_durable_stages,
            degraded,
        }) => {
            processed.read_paths.extend(observed_inputs.paths);
            processed
                .read_time_fingerprints
                .extend(observed_inputs.fingerprints);
            processed.failed_paths.extend(observed_inputs.failed_paths);
            processed.non_durable_stages.extend(non_durable_stages);
            processed.failures.extend(failures);
            processed.uncounted.extend(uncounted);
            processed.stylesheets_measured_separately = degraded;
            // One row for the kind however many artifacts produced it: each was compressed on its
            // own, so their numbers add (ADR-0005).
            let mut css = AssetContribution {
                kind: AssetKind::Css,
                raw_bytes: 0,
                minified_bytes: 0,
                gzip_bytes: 0,
                brotli_bytes: 0,
                zstd_bytes: 0,
            };
            for (bundle, compressed) in counted {
                // Only a TRANSIENT failure makes the result request-local. A missing file is a
                // deterministic fact about the package and stays cacheable.
                let had_asset_io = bundle.failed_paths.iter().any(|failed| !failed.missing)
                    || !bundle.referenced_failures.is_empty();
                css.raw_bytes += bundle.raw_bytes.len() as u64;
                css.minified_bytes += bundle.minified_bytes.len() as u64;
                css.gzip_bytes += compressed.gzip_bytes;
                css.brotli_bytes += compressed.brotli_bytes;
                css.zstd_bytes += compressed.zstd_bytes;
                processed.read_paths.extend(bundle.read_paths);
                processed
                    .read_time_fingerprints
                    .extend(bundle.read_time_fingerprints);
                processed.failed_paths.extend(bundle.failed_paths);
                if had_asset_io {
                    processed
                        .non_durable_stages
                        .insert(crate::engine::stage::ASSET_IO);
                }
                referenced_assets.extend(bundle.referenced_assets);
                for failure in bundle.referenced_failures {
                    processed.read_paths.push(failure.path.clone());
                    processed.failed_paths.push(FailedRead {
                        path: failure.path.clone(),
                        missing: false,
                    });
                    processed.failures.push(failure.message);
                    processed.uncounted.push(UncountedAsset {
                        path: failure.path,
                        bytes: failure.raw_bytes,
                    });
                }
                // Shipped bytes outside the counted taxonomy. Their size IS known, so they are
                // disclosed as ordinary uncounted assets and named in the same summary.
                for asset in bundle.referenced_uncounted {
                    processed.failures.push(format!(
                        "CSS resource {} is not a counted asset kind, so its bytes are disclosed \
                         rather than included",
                        asset.path.display()
                    ));
                    processed.uncounted.push(asset);
                }
                processed
                    .css_dependency_omissions
                    .extend(bundle.dependency_omissions);
                processed
                    .css_dependency_external
                    .extend(bundle.dependency_external);
            }
            processed.contributions.push(css);
        }
        Err(error) => {
            processed.read_paths.extend(error.inputs.paths);
            processed
                .read_time_fingerprints
                .extend(error.inputs.fingerprints);
            processed.failed_paths.extend(error.inputs.failed_paths);
            processed
                .non_durable_stages
                .extend(error.non_durable_stages);
            processed.failures.push(error.message);
            processed.uncounted.extend(
                assets
                    .iter()
                    .filter(|asset| asset.kind == AssetKind::Css)
                    .map(|asset| UncountedAsset {
                        path: asset.path.clone(),
                        bytes: asset.raw_bytes(),
                    }),
            );
        }
    }

    Ok(referenced_assets)
}

/// Compress a bundled stylesheet as its own artifact — never concatenated with anything else first,
/// because it ships as its own file (ADR-0005).
fn compress_bundle(
    bundle: CssBundle,
    context: &AssetProcessingContext,
) -> Result<(CssBundle, CompressionSizes), CssProcessingError> {
    compress_bundle_with(bundle, context, &compress_asset_bytes)
}

fn compress_asset_bytes(bytes: &[u8]) -> Result<CompressionSizes, String> {
    compress_all_bytes(bytes).map_err(|error| error.to_string())
}

fn compress_bundle_with(
    bundle: CssBundle,
    context: &AssetProcessingContext,
    compress: &dyn Fn(&[u8]) -> Result<CompressionSizes, String>,
) -> Result<(CssBundle, CompressionSizes), CssProcessingError> {
    context.check_deadline().map_err(|error| {
        CssProcessingError::from_transform(error.to_string(), CssReadInputs::default())
    })?;
    match compress(&bundle.minified_bytes) {
        Ok(compressed) => {
            context.check_deadline().map_err(|error| {
                CssProcessingError::from_transform(error.to_string(), CssReadInputs::default())
            })?;
            Ok((bundle, compressed))
        }
        Err(error) => {
            let mut inputs = CssReadInputs {
                paths: bundle.read_paths,
                fingerprints: bundle.read_time_fingerprints,
                failed_paths: bundle.failed_paths,
            };
            for asset in bundle.referenced_assets {
                inputs.paths.push(asset.path.clone());
                inputs.fingerprints.push(asset.fingerprint);
            }
            for failure in bundle.referenced_failures {
                inputs.paths.push(failure.path.clone());
                inputs.failed_paths.push(FailedRead {
                    path: failure.path,
                    missing: false,
                });
            }
            Err(CssProcessingError::from_compression(
                format!("failed to compress the bundled stylesheet: {error}"),
                inputs,
            ))
        }
    }
}

fn process_binary_kind(
    assets: &[CollectedAsset],
    kind: AssetKind,
    processed: &mut ProcessedAssets,
    context: &AssetProcessingContext,
) -> Result<(), AssetBudgetFailure> {
    process_binary_kind_with(assets, kind, processed, context, &compress_asset_bytes)
}

fn process_binary_kind_with(
    assets: &[CollectedAsset],
    kind: AssetKind,
    processed: &mut ProcessedAssets,
    context: &AssetProcessingContext,
    compress: &dyn Fn(&[u8]) -> Result<CompressionSizes, String>,
) -> Result<(), AssetBudgetFailure> {
    let mut sizes = MeasuredSizes::ZERO;
    let mut counted = false;

    for asset in assets.iter().filter(|asset| asset.kind == kind) {
        if context.check_deadline().is_err() {
            return Err(context
                .failure()
                .expect("an expired asset deadline must retain a typed failure"));
        }
        let measured = compress(asset.bytes())
            .map_err(|error| format!("failed to compress {}: {error}", asset.path.display()))
            .map(|compressed| (asset.raw_bytes(), compressed));

        match measured {
            Ok((length, compressed)) => {
                counted = true;
                sizes.raw_bytes += length;
                // Nothing to minify in a binary: its shipped size before compression IS its bytes.
                sizes.minified_bytes += length;
                sizes.gzip_bytes += compressed.gzip_bytes;
                sizes.brotli_bytes += compressed.brotli_bytes;
                sizes.zstd_bytes += compressed.zstd_bytes;
            }
            Err(message) => {
                processed
                    .non_durable_stages
                    .insert(crate::pipeline::stage::COMPRESSION);
                processed.failures.push(message);
                processed.uncounted.push(UncountedAsset {
                    path: asset.path.clone(),
                    bytes: asset.raw_bytes(),
                });
            }
        }
        if context.check_deadline().is_err() {
            return Err(context
                .failure()
                .expect("an expired asset deadline must retain a typed failure"));
        }
    }

    if counted {
        processed.contributions.push(AssetContribution {
            kind,
            raw_bytes: sizes.raw_bytes,
            minified_bytes: sizes.minified_bytes,
            gzip_bytes: sizes.gzip_bytes,
            brotli_bytes: sizes.brotli_bytes,
            zstd_bytes: sizes.zstd_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A temp workspace that writes its files up front and deletes itself on drop, so an assertion
    /// failing mid-test cannot leak the directory the way a trailing `remove_dir_all` did.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        /// `Fixture::new("css", &[("index.css", "@import \"./child.css\";"), ("child.css", "…")])`.
        /// A name may contain `/`; its parent directories are created for it.
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("il-assets-{}-{tag}-{unique}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");
            let fixture = Self { dir };
            for (name, contents) in files {
                fixture.write(name, contents);
            }
            fixture
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            self.write_bytes(name, contents.as_bytes())
        }

        fn write_bytes(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("fixture parent");
            }
            fs::write(&path, bytes).expect("fixture file");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn css_asset(path: &Path) -> CollectedAsset {
        read_collected_asset(path, AssetKind::Css).expect("stylesheet snapshot")
    }

    #[test]
    fn bundle_css_inlines_the_import_tree_minifies_and_captures_every_read_path() {
        let fixture = Fixture::new(
            "css",
            &[
                ("child.css", "  .child   {   color :  red ;  }\n"),
                (
                    "index.css",
                    "@import \"./child.css\";\n.entry  {  color :  blue ;  }\n",
                ),
            ],
        );

        let bundle =
            bundle_css(&fixture.path("index.css")).expect("a valid stylesheet should bundle");

        let css = String::from_utf8(bundle.minified_bytes.clone()).expect("utf8");
        // The `@import` child is inlined, both rules survive, and the whitespace is minified away.
        assert!(
            css.contains(".child"),
            "the @import child must be inlined: {css}"
        );
        assert!(css.contains(".entry"), "the entry rule must survive: {css}");
        assert!(!css.contains("  "), "output must be minified: {css}");
        assert!(
            bundle.raw_bytes.len() > bundle.minified_bytes.len(),
            "the unminified print must be larger than the minified one",
        );
        // Both the entry and the @import child are captured for cache freshness.
        assert!(
            bundle
                .read_paths
                .iter()
                .any(|path| path.ends_with("index.css")),
            "the entry must be captured: {:?}",
            bundle.read_paths,
        );
        assert!(
            bundle
                .read_paths
                .iter()
                .any(|path| path.ends_with("child.css")),
            "the @import child must be captured: {:?}",
            bundle.read_paths,
        );
    }

    #[test]
    fn invalid_utf8_is_a_deterministic_css_input_whether_top_level_or_imported() {
        let fixture = Fixture::new(
            "invalid-utf8-css",
            &[(
                "importing.css",
                "@import './child.css';\n.root { color: red; }",
            )],
        );
        fixture.write_bytes("child.css", &[0xff, 0xfe, 0xfd]);
        let top_level = fixture.write_bytes("top-level.css", &[0xff, 0xfe, 0xfd]);

        let imported_failure =
            process_assets_for_test(&[css_asset(&fixture.path("importing.css"))]);
        let top_level_failure = process_assets_for_test(&[css_asset(&top_level)]);

        for (label, processed) in [
            ("imported child", imported_failure),
            ("top-level entry", top_level_failure),
        ] {
            let diagnostics = asset_diagnostics(&processed);
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.stage == diagnostic_stage::UNCOUNTED_ASSETS),
                "{label}: invalid CSS is disclosed as a deterministic processor fallback"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.stage != crate::engine::stage::ASSET_IO),
                "{label}: bytes were read exactly; invalid UTF-8 is not a filesystem failure"
            );
            assert!(
                crate::cache::key::fingerprints_are_reusable(&processed.freshness_fingerprints()),
                "{label}: the deterministic fallback should expire against the exact invalid bytes"
            );
        }
    }

    #[test]
    fn dependency_analysis_discovers_a_font_without_rewriting_measured_css() {
        let fixture = Fixture::new(
            "css-font-url",
            &[(
                "index.css",
                "@font-face { font-family: Probe; src: url('./probe.woff2'); }\n",
            )],
        );
        let font = fixture.write_bytes("probe.woff2", &[0x5a; 32]);

        let expected_font = read_collected_asset(&font, AssetKind::Font).expect("font snapshot");
        let bundle =
            bundle_css(&fixture.path("index.css")).expect("a valid stylesheet should bundle");

        let css = std::str::from_utf8(&bundle.minified_bytes).expect("utf8");
        assert!(
            css.contains("probe.woff2"),
            "dependency-analysis placeholders must never enter the measured artifact: {css}"
        );
        assert_eq!(bundle.referenced_assets, vec![expected_font]);
        assert!(bundle.dependency_omissions.is_empty());
    }

    /// `@import "theme.css"` — no `./` — is a RELATIVE url in CSS, and one of the most common shapes
    /// real stylesheets ship. Deciding by spelling alone would have dropped every such sheet to
    /// raw-byte disclosure; the file's existence beside the sheet is what decides.
    #[test]
    fn an_unprefixed_import_is_relative_to_the_sheet_and_is_counted() {
        let fixture = Fixture::new(
            "unprefixed-import",
            &[
                ("theme.css", ".theme { color: blue }\n"),
                ("sub/dir.css", ".sub { color: green }\n"),
                (
                    "index.css",
                    "@import \"theme.css\";\n@import \"sub/dir.css\";\n.a { color: red }\n",
                ),
            ],
        );

        let bundle = bundle_css(&fixture.path("index.css"))
            .expect("an unprefixed relative @import must resolve beside the sheet");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");
        assert!(
            css.contains(".theme") && css.contains(".sub") && css.contains(".a"),
            "every relative sheet must be inlined and counted: {css}"
        );
    }

    /// **The daemon-killer.** Lightning CSS cycle-detects on the path spelling `resolve` hands back,
    /// and the built-in resolver's naive `with_file_name` join never normalizes `..`, so a cycle
    /// crossing a `../` yielded a distinct key for the same file on every hop, the dedup never
    /// fired, and the recursion overflowed the stack — uncatchable, `__fastfail`, every in-flight
    /// request dead with it.
    ///
    /// If this test ever hangs or aborts the runner rather than failing, THAT is the regression.
    #[test]
    fn a_dot_dot_crossing_import_cycle_terminates_instead_of_killing_the_process() {
        // A mutual cycle that crosses `../` in both directions.
        let fixture = Fixture::new(
            "cycle",
            &[
                (
                    "components/button.css",
                    "@import \"../theme/tokens.css\";\n.button { color: red }\n",
                ),
                (
                    "theme/tokens.css",
                    "@import \"../components/button.css\";\n:root { --x: 1 }\n",
                ),
                ("other.css", ".other { color: teal }\n"),
            ],
        );

        // The multi-entry path is the exposed one: it is what the synthetic entry serves.
        let canonical =
            |name: &str| std::fs::canonicalize(fixture.path(name)).expect("canonicalize");
        let result = bundle_css_set(&[canonical("components/button.css"), canonical("other.css")]);

        // Terminating at all is the whole assertion: reaching this line means the process survived.
        let bundle = result.expect("a cyclic @import must terminate, not overflow the stack");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");
        // Sheets outside the cycle are still counted, so one package's broken CSS cannot sink the
        // set. Lightning CSS drops the cyclic sheet's own rules, undercounting that one stylesheet —
        // recorded in known-issues.
        assert!(
            css.contains(".other"),
            "a stylesheet outside the cycle must still be counted: {css}"
        );
    }

    /// The file count bounds BOTH breadth and depth because it is the only bound available: giving
    /// the walk its own big stack does NOT work, since Lightning CSS recurses on rayon workers whose
    /// stacks it does not own.
    #[test]
    fn a_stylesheet_tree_past_the_file_budget_is_refused_rather_than_read_forever() {
        let fixture = Fixture::new("budget", &[]);
        let leaves = MAX_STYLESHEET_FILES + 8;
        let mut entry = String::new();
        for index in 0..leaves {
            fixture.write(
                &format!("leaf{index}.css"),
                &format!(".rule{index} {{ color: red }}\n"),
            );
            entry.push_str(&format!("@import \"./leaf{index}.css\";\n"));
        }
        fixture.write("index.css", &entry);

        let error = bundle_css(&fixture.path("index.css"))
            .expect_err("a tree past the file budget must be refused");
        assert!(error.contains("limit"), "{error}");
    }

    /// The other half of the bound: it must refuse the absurd without refusing the real. It stays
    /// shallow deliberately — a test that recursed near the budget would overflow a DEBUG build's
    /// stack, whose frames run an order of magnitude larger than the release build it is sized for.
    #[test]
    fn an_ordinary_import_chain_inside_the_budget_still_bundles() {
        let fixture = Fixture::new("chain", &[]);
        let depth = 24;
        for index in 0..depth {
            let next = if index + 1 < depth {
                format!("@import \"./sheet{}.css\";\n", index + 1)
            } else {
                String::new()
            };
            fixture.write(
                &format!("sheet{index}.css"),
                &format!("{next}.rule{index} {{ color: red }}\n"),
            );
        }

        let bundle =
            bundle_css(&fixture.path("sheet0.css")).expect("an ordinary chain must bundle");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");
        assert!(css.contains(".rule0") && css.contains(".rule23"), "{css}");
    }

    /// A remote `@import` has no file behind it. A real bundler leaves it in the sheet; treating it
    /// as a resolve failure would sink every stylesheet in the set to raw disclosure over a shape
    /// ordinary packages ship.
    #[test]
    fn a_remote_import_is_external_and_does_not_sink_the_stylesheet() {
        let fixture = Fixture::new(
            "remote",
            &[(
                "index.css",
                "@import url(\"https://fonts.googleapis.com/css2?family=Inter\");\n.a { color: red }\n",
            )],
        );

        let bundle = bundle_css(&fixture.path("index.css"))
            .expect("a remote @import must not fail the stylesheet");
        let css = std::str::from_utf8(&bundle.minified_bytes).expect("utf8");
        assert!(
            css.contains(".a"),
            "the local rules must still be counted: {css}"
        );
        // A CDN stylesheet is fetched at runtime and is not a byte this package ships, so the
        // measured size is EXACT and must keep its budget verdict. Disclosing it on a precision
        // stage instead silently disabled budgeting for every package that `@import`s a web font.
        assert_eq!(
            bundle.dependency_external.len(),
            1,
            "the external stylesheet must still be disclosed: {bundle:?}"
        );
        assert!(
            bundle.dependency_external[0].contains("fonts.googleapis.com"),
            "the disclosure should identify the external stylesheet: {bundle:?}"
        );
        assert!(
            bundle.dependency_omissions.is_empty(),
            "an external @import is out of scope, not a local omission: {bundle:?}"
        );

        let mut processed = ProcessedAssets::default();
        processed
            .css_dependency_external
            .extend(bundle.dependency_external);
        assert!(
            !processed.has_uncounted_assets(),
            "external weight must not make an exact measurement a floor"
        );
        let diagnostic = external_css_resources_diagnostic(&processed)
            .expect("the external reference must be disclosed to the user");
        assert_eq!(
            diagnostic.stage,
            diagnostic_stage::EXTERNAL,
            "{diagnostic:?}"
        );
        assert!(
            !crate::pipeline::stage::prevents_budget_verdict(&diagnostic.stage),
            "an exact size must keep its budget verdict: {diagnostic:?}"
        );
    }

    /// The headline claims to be the import's full cost, so a shipped file it does not include has
    /// to say so. An image is outside the counted taxonomy, which used to mean it left through a
    /// bare `None` — out of the number, out of every disclosure, still High confidence.
    #[test]
    fn an_image_referenced_by_css_is_disclosed_with_its_real_size() {
        let fixture = Fixture::new(
            "image-url",
            &[("index.css", ".a { background-image: url('./bg.png') }\n")],
        );
        fixture.write_bytes("bg.png", &[7u8; 4096]);

        let bundle = bundle_css(&fixture.path("index.css"))
            .expect("an image reference must not fail the stylesheet");
        assert_eq!(
            bundle.referenced_uncounted.len(),
            1,
            "the shipped image must be disclosed: {bundle:?}"
        );
        assert_eq!(
            bundle.referenced_uncounted[0].bytes, 4096,
            "the disclosure must carry the image's real size, not a zero: {bundle:?}"
        );
        assert!(
            bundle.dependency_omissions.is_empty(),
            "a resolvable image is a sized disclosure, not an unknown omission: {bundle:?}"
        );
    }

    /// A `url()` in a custom property fails lightningcss's dependency print while both measuring
    /// prints succeed, so the sheet is counted with its whole `url()` graph undiscovered — and one
    /// such declaration disables discovery for every sheet in the union. The omission has to reach
    /// `has_uncounted_assets` or the short total is cached as complete.
    #[test]
    fn an_uninspectable_url_graph_is_an_omission_not_an_over_count() {
        let fixture = Fixture::new(
            "ambiguous-url",
            &[(
                "index.css",
                ":root { --icon-font: url(./icons.woff2) }\n.a { color: red }\n",
            )],
        );
        fixture.write_bytes("icons.woff2", &[3u8; 2048]);

        let bundle = bundle_css(&fixture.path("index.css"))
            .expect("an ambiguous url() must not fail the stylesheet");
        assert!(
            !bundle.minified_bytes.is_empty(),
            "the sheet itself must still be measured: {bundle:?}"
        );
        assert_eq!(
            bundle.dependency_omissions.len(),
            1,
            "an uninspectable url() graph must be disclosed as an omission: {bundle:?}"
        );
        // Pin the mechanism, not just the symptom: this fires because the metadata-only print is
        // the only one with dependency analysis on, so it is the only one that can fail here.
        assert!(
            bundle.dependency_omissions[0].contains("could not inspect resource URLs"),
            "the omission must name the dependency print that failed: {bundle:?}"
        );
        assert!(
            bundle.referenced_assets.is_empty(),
            "the font behind the ambiguous url() is exactly what went undiscovered: {bundle:?}"
        );

        let mut processed = ProcessedAssets::default();
        processed
            .css_dependency_omissions
            .extend(bundle.dependency_omissions);
        assert!(
            processed.has_uncounted_assets(),
            "the omission must make the result a floor so it cannot be cached as complete"
        );
        let diagnostic = omitted_css_resources_diagnostic(&processed)
            .expect("the omission must be disclosed to the user");
        assert_eq!(
            diagnostic.stage,
            diagnostic_stage::UNCOUNTED_ASSETS,
            "bytes MISSING are uncounted, not imprecise: {diagnostic:?}"
        );
    }

    /// One unprocessable sheet must not take the others down with it. The set spans every import in
    /// the runtime group, so all-or-nothing meant one package's `.scss` silently reverted CSS
    /// counting for all of them.
    #[test]
    fn one_unparseable_stylesheet_does_not_sink_the_rest_of_the_set() {
        let fixture = Fixture::new(
            "isolation",
            &[
                ("good.css", ".good { color: red }\n"),
                // Real preprocessor syntax: Lightning CSS parses plain CSS only.
                (
                    "bad.scss",
                    "$brand: red;\n@mixin thing { color: $brand }\n.bad { @include thing }\n",
                ),
            ],
        );
        let assets = vec![
            css_asset(&fixture.path("good.css")),
            css_asset(&fixture.path("bad.scss")),
        ];
        let expected_bad = assets[1].path.clone();

        let processed = process_assets(
            &assets,
            test_context_with(&assets, AssetBudgetLimits::unbounded_css_work()),
        )
        .expect("the per-attempt bound is what this test exercises");

        let css = processed
            .contributions
            .iter()
            .find(|contribution| contribution.kind == AssetKind::Css)
            .expect("the parseable stylesheet must still be counted");
        assert!(css.brotli_bytes > 0, "{css:?}");
        assert_eq!(
            processed
                .uncounted
                .iter()
                .map(|asset| &asset.path)
                .collect::<Vec<_>>(),
            vec![&expected_bad],
            "only the offender falls back to disclosure: {processed:?}",
        );
        assert!(
            processed.has_uncounted_assets(),
            "the aggregate must be able to distinguish this missing-byte result"
        );
    }

    /// When the union AND every per-sheet retry fail, each stylesheet must be disclosed ONCE. The
    /// retry used to report each failure as it went and then hand back an error, so the outer arm
    /// disclosed them all a second time and the diagnostic doubled its own count and byte total — a
    /// wrong number in the one place that exists to be honest about what is missing.
    #[test]
    fn a_set_where_every_stylesheet_fails_discloses_each_of_them_exactly_once() {
        let fixture = Fixture::new(
            "allfail",
            &[
                (
                    "one.scss",
                    "$a: red;\n@mixin m { color: $a }\n.x { @include m }\n",
                ),
                (
                    "two.scss",
                    "$b: blue;\n@mixin n { color: $b }\n.y { @include n }\n",
                ),
            ],
        );
        let assets: Vec<CollectedAsset> = ["one.scss", "two.scss"]
            .iter()
            .map(|name| css_asset(&fixture.path(name)))
            .collect();
        let expected_paths = assets
            .iter()
            .map(|asset| asset.path.clone())
            .collect::<Vec<_>>();

        let processed = process_assets_for_test(&assets);

        assert!(
            processed.contributions.is_empty(),
            "nothing parsed, so nothing may be counted: {processed:?}",
        );
        assert_eq!(
            processed.uncounted.len(),
            2,
            "each stylesheet is disclosed exactly once, never twice: {processed:?}",
        );
        let mut disclosed: Vec<_> = processed
            .uncounted
            .iter()
            .map(|asset| &asset.path)
            .collect();
        disclosed.sort();
        assert_eq!(
            disclosed,
            expected_paths.iter().collect::<Vec<_>>(),
            "{processed:?}"
        );
    }

    /// The union can fail for a reason NO individual sheet fails for, and then every sheet counts and
    /// `uncounted` is empty. That combination used to produce an over-counted size (a shared
    /// `@import` inlined into each sheet) carrying NO diagnostic, so it read as High confidence and
    /// was written to disk. The over-count is the accepted cost of degrading; the silence was not.
    #[test]
    fn a_set_that_degrades_to_per_sheet_still_discloses_that_it_may_read_high() {
        let fixture = Fixture::new("degraded", &[("shared.css", ".shared { color: red }\n")]);
        // Each sheet's own tree is well inside the budget; only the two together breach it, which
        // is what makes the union fail while each sheet on its own succeeds.
        let per_sheet_leaves = 140;

        let entries: Vec<PathBuf> = ["a", "b"]
            .iter()
            .map(|name| {
                let mut source = String::from("@import \"./shared.css\";\n");
                for index in 0..per_sheet_leaves {
                    let leaf = format!("{name}{index}.css");
                    fixture.write(&leaf, &format!(".r{name}{index} {{ color: red }}\n"));
                    source.push_str(&format!("@import \"./{leaf}\";\n"));
                }
                fixture.write(&format!("{name}.css"), &source)
            })
            .collect();

        let assets: Vec<CollectedAsset> = entries.iter().map(|path| css_asset(path)).collect();

        // Guard the premise: if this ever stops being the shape under test, the assertions below
        // would pass for the wrong reason.
        assert!(
            bundle_collected_css_set(
                &assets,
                test_context_with(&assets, AssetBudgetLimits::unbounded_css_work())
            )
            .is_err(),
            "the premise is a set whose union breaches the budget",
        );
        assert!(
            bundle_css(&entries[0]).is_ok(),
            "the premise is that each sheet on its own is well inside the budget",
        );

        let processed = process_assets(
            &assets,
            test_context_with(&assets, AssetBudgetLimits::unbounded_css_work()),
        )
        .expect("the per-attempt bound is what this test exercises");

        assert!(
            processed
                .contributions
                .iter()
                .any(|contribution| contribution.kind == AssetKind::Css
                    && contribution.raw_bytes > 0),
            "every sheet still counts when the union degrades: {processed:?}",
        );
        assert!(
            processed.uncounted.is_empty(),
            "nothing failed, so nothing is uncounted - which is exactly why the uncounted \
             disclosure cannot be what reports this: {processed:?}",
        );
        assert!(
            !processed.has_uncounted_assets(),
            "the per-sheet result reads high but does not omit a stylesheet"
        );
        assert!(
            uncounted_assets_diagnostic(&processed).is_none(),
            "there is nothing uncounted to report: {processed:?}",
        );

        let disclosure =
            imprecise_assets_diagnostic(&processed).expect("the over-count must be disclosed");
        assert_eq!(disclosure.stage, diagnostic_stage::IMPRECISE_ASSETS);
        assert!(
            !disclosure.details.is_empty(),
            "the disclosure must carry why the union failed: {disclosure:?}",
        );
        // The disclosure is what drops confidence off High, so a caller taking every diagnostic is
        // what makes the number honest.
        assert_eq!(
            asset_diagnostics(&processed).len(),
            1,
            "exactly one disclosure, never zero and never doubled: {processed:?}",
        );
    }

    #[test]
    fn a_failed_css_read_remains_unverifiable_after_a_later_success() {
        let fixture = Fixture::new("failed-then-readable", &[]);
        let child = fixture.path("created.css");
        let provider = TrackingProvider::new(&[], None, test_context(&[]));

        assert!(provider.read(&child).is_err(), "the first read is missing");
        fixture.write("created.css", ".created { color: red }");
        assert!(provider.read(&child).is_ok(), "the retry can read it");

        let inputs = provider.into_read_inputs();
        let processed = ProcessedAssets {
            read_paths: inputs.paths,
            read_time_fingerprints: inputs.fingerprints,
            failed_paths: inputs.failed_paths,
            ..ProcessedAssets::default()
        };
        let freshness = processed.freshness_fingerprints();

        // Asserts the INVARIANT rather than which sentinel carries it. This run saw one path in two
        // states — absent, then present with bytes — and a run that disagrees with itself cannot be
        // reused whichever marker records the first observation.
        assert!(
            !crate::cache::key::fingerprints_are_reusable(&freshness),
            "a later success must not make a mixed failure/success run cacheable: {freshness:?}"
        );
        assert!(
            freshness
                .iter()
                .any(|fingerprint| fingerprint.content_hash.is_some()),
            "the successful retry should still retain its exact snapshot: {freshness:?}"
        );
    }

    #[test]
    fn a_css_compressor_failure_is_typed_non_durable_and_does_not_split_the_artifact() {
        let bundle = CssBundle {
            raw_bytes: b".a { color: red }".to_vec(),
            minified_bytes: b".a{color:red}".to_vec(),
            read_paths: Vec::new(),
            read_time_fingerprints: Vec::new(),
            failed_paths: Vec::new(),
            referenced_assets: Vec::new(),
            referenced_failures: Vec::new(),
            referenced_uncounted: Vec::new(),
            dependency_omissions: Vec::new(),
            dependency_external: Vec::new(),
        };
        let failure = compress_bundle_with(bundle, &test_context(&[]), &|_| {
            Err("injected failure".to_owned())
        })
        .expect_err("the injected compressor must fail");

        assert!(
            failure
                .non_durable_stages
                .contains(crate::pipeline::stage::COMPRESSION),
            "the cache gate must receive a typed cause, never parse this message: {failure:?}"
        );
        assert!(
            !may_retry_stylesheets_separately(&failure),
            "compressor failure says nothing about CSS structure and must not split one artifact"
        );
    }

    #[test]
    fn a_binary_compressor_failure_is_disclosed_and_non_durable() {
        let fixture = Fixture::new("binary-compression-failure", &[]);
        let path = fixture.write_bytes("probe.woff2", &[0x51; 64]);
        let asset = read_collected_asset(&path, AssetKind::Font).expect("font snapshot");
        let mut processed = ProcessedAssets::default();

        process_binary_kind_with(
            &[asset],
            AssetKind::Font,
            &mut processed,
            &test_context(&[]),
            &|_| Err("injected failure".to_owned()),
        )
        .expect("a per-asset compressor failure falls back instead of aborting the stage");

        assert_eq!(processed.uncounted.len(), 1);
        assert!(processed.contributions.is_empty());
        assert!(
            asset_diagnostics(&processed)
                .iter()
                .any(|diagnostic| diagnostic.stage == crate::pipeline::stage::COMPRESSION),
            "the measured fallback must carry the cause that keeps it out of every store"
        );
    }

    /// Several reachable stylesheets are ONE artifact, and a rule they both `@import` is inlined
    /// once — not summed twice, which is what bundling them separately would do.
    #[test]
    fn bundle_css_set_unions_several_stylesheets_and_dedupes_a_shared_import() {
        let fixture = Fixture::new(
            "set",
            &[
                ("shared.css", ".shared { color: red }\n"),
                ("a.css", "@import \"./shared.css\";\n.a { color: blue }\n"),
                ("b.css", "@import \"./shared.css\";\n.b { color: green }\n"),
            ],
        );

        let bundle = bundle_css_set(&[fixture.path("a.css"), fixture.path("b.css")])
            .expect("both stylesheets should bundle");

        let css = String::from_utf8(bundle.minified_bytes.clone()).expect("utf8");
        assert!(css.contains(".a"), "the first sheet must be inlined: {css}");
        assert!(
            css.contains(".b"),
            "the second sheet must be inlined: {css}"
        );
        assert_eq!(
            css.matches(".shared").count(),
            1,
            "a stylesheet both sheets @import must be inlined ONCE, not counted twice: {css}",
        );
        for name in ["shared.css", "a.css", "b.css"] {
            assert!(
                bundle.read_paths.iter().any(|path| path.ends_with(name)),
                "{name} must be captured for freshness: {:?}",
                bundle.read_paths,
            );
        }
    }

    #[test]
    fn process_assets_counts_a_stylesheet_and_reports_its_import_child_for_freshness() {
        let fixture = Fixture::new(
            "process",
            &[
                ("child.css", ".child { color: red }\n"),
                (
                    "index.css",
                    "@import \"./child.css\";\n.entry { color: blue }\n",
                ),
            ],
        );

        let processed = process_assets_for_test(&[css_asset(&fixture.path("index.css"))]);

        assert_eq!(processed.contributions.len(), 1, "{processed:?}");
        let contribution = &processed.contributions[0];
        assert_eq!(contribution.kind, AssetKind::Css);
        assert!(
            contribution.brotli_bytes > 0 && contribution.minified_bytes > 0,
            "a stylesheet must contribute real bytes: {contribution:?}",
        );
        assert!(processed.uncounted.is_empty(), "{processed:?}");
        assert!(
            processed
                .read_paths
                .iter()
                .any(|path| path.ends_with("child.css")),
            "the @import child must reach freshness: {:?}",
            processed.read_paths,
        );
        assert_eq!(processed.total().brotli_bytes, contribution.brotli_bytes);
    }

    /// The fallback that keeps B2 from ever being worse than what it replaced. A dangling `@import`
    /// cannot be resolved from disk, so bundling must error rather than panic and the caller reverts
    /// to raw-byte disclosure.
    #[test]
    fn process_assets_falls_back_to_raw_disclosure_when_a_stylesheet_cannot_be_processed() {
        let fixture = Fixture::new(
            "fallback",
            &[(
                "index.css",
                "@import \"./missing.css\";\n.a { color: red }\n",
            )],
        );
        let asset = css_asset(&fixture.path("index.css"));

        let processed = process_assets_for_test(std::slice::from_ref(&asset));
        let expected_path = asset.path.clone();
        let expected_bytes = asset.raw_bytes();

        assert!(
            processed.contributions.is_empty(),
            "an unprocessable stylesheet must not be counted: {processed:?}",
        );
        assert_eq!(
            processed.uncounted,
            vec![UncountedAsset {
                path: expected_path,
                bytes: expected_bytes
            }],
            "it must be disclosed with its raw bytes, exactly as before B2: {processed:?}",
        );
        assert!(!processed.failures.is_empty(), "{processed:?}");
    }

    #[test]
    fn process_assets_counts_binary_assets_raw_and_sums_each_kind() {
        let fixture = Fixture::new("binary", &[]);
        // Deliberately compressible so gzip/brotli/zstd all produce a real number.
        let wasm = fixture.write_bytes("engine.wasm", &[7_u8; 4096]);
        let font = fixture.write_bytes("body.woff2", &[9_u8; 2048]);
        let assets = vec![
            read_collected_asset(&wasm, AssetKind::Wasm).expect("wasm snapshot"),
            read_collected_asset(&font, AssetKind::Font).expect("font snapshot"),
        ];

        let processed = process_assets_for_test(&assets);

        assert_eq!(processed.contributions.len(), 2, "{processed:?}");
        let wasm_contribution = processed
            .contributions
            .iter()
            .find(|contribution| contribution.kind == AssetKind::Wasm)
            .expect("wasm contribution");
        // A binary has nothing to minify: its pre-compression size is its bytes.
        assert_eq!(wasm_contribution.raw_bytes, 4096);
        assert_eq!(wasm_contribution.minified_bytes, 4096);
        assert!(wasm_contribution.brotli_bytes > 0);
        assert_eq!(processed.total().raw_bytes, 4096 + 2048);
        assert!(processed.uncounted.is_empty(), "{processed:?}");
    }
}
