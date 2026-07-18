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

use crate::cache::key::{
    FileFingerprint, content_hash, file_fingerprint_from_read_time, read_time_len_mtime_of,
    sort_and_dedup_fingerprints,
};
use crate::engine::{
    AssetKind, CollectedAsset, UncountedAsset, diagnostic_stage, read_collected_asset,
};
use crate::ipc::protocol::{AssetContribution, ImportDiagnostic, MeasuredSizes};
use crate::pipeline::asset_boundary::{self, AssetBoundaryError, AssetDeadline};
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
    failed_paths: Mutex<HashSet<PathBuf>>,
    budget: Mutex<ReadBudget>,
    /// One ledger for every union/per-sheet attempt in this build. `None` only for the small
    /// standalone helpers used by processor tests; daemon production always supplies it.
    context: Option<Arc<AssetProcessingContext>>,
}

impl TrackingProvider {
    fn new(entries: &[CollectedAsset]) -> Self {
        Self {
            inner: FileProvider::new(),
            retained_sources: RetainedSources::default(),
            preloaded: entries
                .iter()
                .cloned()
                .map(|asset| (asset.path.clone(), asset))
                .collect(),
            synthetic: None,
            read_paths: Mutex::new(HashSet::new()),
            read_time_fingerprints: Mutex::new(Vec::new()),
            failed_paths: Mutex::new(HashSet::new()),
            budget: Mutex::new(ReadBudget::default()),
            context: None,
        }
    }

    fn new_bounded(entries: &[CollectedAsset], context: Arc<AssetProcessingContext>) -> Self {
        let mut preloaded = context
            .snapshots()
            .into_iter()
            .map(|asset| (asset.path.clone(), asset))
            .collect::<BTreeMap<_, _>>();
        preloaded.extend(
            entries
                .iter()
                .cloned()
                .map(|asset| (asset.path.clone(), asset)),
        );
        Self {
            inner: FileProvider::new(),
            retained_sources: RetainedSources::default(),
            preloaded,
            synthetic: None,
            read_paths: Mutex::new(HashSet::new()),
            read_time_fingerprints: Mutex::new(Vec::new()),
            failed_paths: Mutex::new(HashSet::new()),
            budget: Mutex::new(ReadBudget::default()),
            context: Some(context),
        }
    }

    fn with_synthetic(entries: &[CollectedAsset], path: PathBuf, content: String) -> Self {
        Self {
            inner: FileProvider::new(),
            retained_sources: RetainedSources::default(),
            preloaded: entries
                .iter()
                .cloned()
                .map(|asset| (asset.path.clone(), asset))
                .collect(),
            synthetic: Some((path, content)),
            read_paths: Mutex::new(HashSet::new()),
            read_time_fingerprints: Mutex::new(Vec::new()),
            failed_paths: Mutex::new(HashSet::new()),
            budget: Mutex::new(ReadBudget::default()),
            context: None,
        }
    }

    fn with_synthetic_bounded(
        entries: &[CollectedAsset],
        path: PathBuf,
        content: String,
        context: Arc<AssetProcessingContext>,
    ) -> Self {
        let mut preloaded = context
            .snapshots()
            .into_iter()
            .map(|asset| (asset.path.clone(), asset))
            .collect::<BTreeMap<_, _>>();
        preloaded.extend(
            entries
                .iter()
                .cloned()
                .map(|asset| (asset.path.clone(), asset)),
        );
        Self {
            inner: FileProvider::new(),
            retained_sources: RetainedSources::default(),
            preloaded,
            synthetic: Some((path, content)),
            read_paths: Mutex::new(HashSet::new()),
            read_time_fingerprints: Mutex::new(Vec::new()),
            failed_paths: Mutex::new(HashSet::new()),
            budget: Mutex::new(ReadBudget::default()),
            context: Some(context),
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
        failed_paths.sort();
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

    fn record_failed_read(&self, path: PathBuf) {
        self.failed_paths
            .lock()
            .expect("CSS failed-path set should not be poisoned")
            .insert(path.clone());
        if let Some(context) = &self.context {
            context.record_failed_path(&path);
        }
    }

    fn check_deadline(&self) -> Result<(), std::io::Error> {
        self.context
            .as_ref()
            .map_or(Ok(()), |context| context.check_deadline())
    }

    fn read_referenced_asset(
        &self,
        path: &Path,
        kind: AssetKind,
    ) -> std::io::Result<CollectedAsset> {
        match &self.context {
            Some(context) => context.snapshot(path, kind),
            None => read_collected_asset(path, kind),
        }
    }

    fn should_continue_dependency_reads(&self) -> bool {
        self.context
            .as_ref()
            .is_none_or(|context| context.check_deadline().is_ok())
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
        let source = match self.preloaded.get(&key) {
            Some(asset) => {
                if let Some(context) = &self.context {
                    context.charge_css_snapshot(asset)?;
                }
                self.reserve(asset.bytes().len())?;
                self.record_read(key.clone(), Some(asset.fingerprint.clone()));
                std::str::from_utf8(asset.bytes()).map_err(|error| {
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
                        self.record_failed_read(key);
                        return Err(error);
                    }
                };
                let shared_reservation = self
                    .context
                    .as_ref()
                    .map(|context| context.begin_css_read(&key, &metadata))
                    .transpose()?;
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
                        self.record_failed_read(key);
                        return Err(error);
                    }
                };
                self.reconcile(reservation, bytes.len())?;
                let (len, modified_millis) = read_time_len_mtime_of(&metadata);
                let fingerprint = match (&self.context, shared_reservation) {
                    (Some(context), Some(reservation)) => {
                        context.finish_css_read(reservation, &bytes)?.fingerprint
                    }
                    _ => file_fingerprint_from_read_time(
                        &key,
                        len,
                        modified_millis,
                        content_hash(&bytes),
                    ),
                };
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
    failed_paths: Vec<PathBuf>,
    /// Supported local artifacts referenced by the CSS that survives minification. They are
    /// separate emitted files, so the caller processes and compresses them independently.
    pub referenced_assets: Vec<CollectedAsset>,
    /// Local supported resources that survived CSS minification but could not be read.
    pub(crate) referenced_failures: Vec<crate::pipeline::css_dependencies::CssDependencyFailure>,
    /// Dependency analysis is metadata-only and must never discard an otherwise valid CSS size.
    /// When it cannot inspect every URL (for example an ambiguous relative URL in a custom
    /// property), the caller keeps the CSS contribution and discloses that referenced assets may
    /// be missing.
    pub dependency_failures: Vec<String>,
}

#[derive(Debug, Default)]
struct CssReadInputs {
    paths: Vec<PathBuf>,
    fingerprints: Vec<FileFingerprint>,
    failed_paths: Vec<PathBuf>,
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
        if !inputs.failed_paths.is_empty() {
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
pub fn bundle_css(entry: &Path) -> Result<CssBundle, String> {
    let asset = read_collected_asset(entry, AssetKind::Css)
        .map_err(|error| format!("failed to read stylesheet {}: {error}", entry.display()))?;
    bundle_collected_css(&asset).map_err(|error| error.message)
}

fn bundle_collected_css(entry: &CollectedAsset) -> Result<CssBundle, CssProcessingError> {
    bundle_collected_css_with_context(entry, None)
}

fn bundle_collected_css_with_context(
    entry: &CollectedAsset,
    context: Option<Arc<AssetProcessingContext>>,
) -> Result<CssBundle, CssProcessingError> {
    let provider = match context {
        Some(context) => TrackingProvider::new_bounded(std::slice::from_ref(entry), context),
        None => TrackingProvider::new(std::slice::from_ref(entry)),
    };
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
pub fn bundle_css_set(entries: &[PathBuf]) -> Result<CssBundle, String> {
    let entries = entries
        .iter()
        .map(|entry| {
            read_collected_asset(entry, AssetKind::Css)
                .map_err(|error| format!("failed to read stylesheet {}: {error}", entry.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    bundle_collected_css_set(&entries).map_err(|error| error.message)
}

fn bundle_collected_css_set(entries: &[CollectedAsset]) -> Result<CssBundle, CssProcessingError> {
    bundle_collected_css_set_with_context(entries, None)
}

fn bundle_collected_css_set_with_context(
    entries: &[CollectedAsset],
    context: Option<Arc<AssetProcessingContext>>,
) -> Result<CssBundle, CssProcessingError> {
    match entries {
        [] => Err(CssProcessingError {
            message: "no stylesheets to bundle".to_owned(),
            inputs: CssReadInputs::default(),
            non_durable_stages: BTreeSet::new(),
        }),
        [single] => bundle_collected_css_with_context(single, context),
        many => {
            let paths = many
                .iter()
                .map(|asset| asset.path.clone())
                .collect::<Vec<_>>();
            let (path, content) = synthetic_entry(&paths);
            let provider = match context {
                Some(context) => {
                    TrackingProvider::with_synthetic_bounded(many, path.clone(), content, context)
                }
                None => TrackingProvider::with_synthetic(many, path.clone(), content),
            };
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
    let (referenced_assets, referenced_failures, dependency_failures) = stylesheet
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
                dependencies.unresolved,
            )
        })
        .unwrap_or_else(|error| {
            (
                Vec::new(),
                Vec::new(),
                vec![format!(
                    "lightningcss could not inspect resource URLs in {}: {error:?}",
                    entry.display()
                )],
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
        dependency_failures,
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
    failed_paths: Vec<PathBuf>,
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
    /// Dependency-analysis failures keep the valid CSS contribution but make it impossible to
    /// prove that every local font/wasm reference was discovered.
    pub css_dependency_failures: Vec<String>,
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
        !self.uncounted.is_empty()
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
        let mut failed_paths = self.failed_paths.clone();
        // Defensive inference for any future processor that records an attempted path but forgets
        // to classify the failed read explicitly. Explicit failures remain even if another attempt
        // later fingerprinted the same path.
        failed_paths.extend(
            self.read_paths
                .iter()
                .filter(|path| !fingerprinted.contains(&path.to_string_lossy().replace('\\', "/")))
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

    let total_bytes: u64 = processed.uncounted.iter().map(|asset| asset.bytes).sum();
    let names = processed
        .uncounted
        .iter()
        .map(|asset| {
            asset
                .path
                .file_name()
                .unwrap_or(asset.path.as_os_str())
                .to_string_lossy()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ");

    Some(ImportDiagnostic {
        stage: diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
        message: format!(
            "package ships {} non-JavaScript asset(s) totalling {total_bytes} bytes that could not \
             be processed, so this size does NOT include them: {names}",
            processed.uncounted.len()
        ),
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

fn unresolved_css_dependencies_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    if processed.css_dependency_failures.is_empty() {
        return None;
    }

    Some(ImportDiagnostic {
        stage: diagnostic_stage::IMPRECISE_ASSETS.to_owned(),
        message: "the stylesheet was measured, but some CSS resource references could not be \
                  inspected, so this size may omit referenced font or wasm artifacts"
            .to_owned(),
        details: processed.css_dependency_failures.clone(),
    })
}

fn asset_io_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    if processed.failed_paths.is_empty() {
        return None;
    }
    let mut paths = processed.failed_paths.clone();
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
        unresolved_css_dependencies_diagnostic(processed),
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
pub(crate) struct AssetProcessingFailure {
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
pub(crate) fn process_assets_bounded(
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
        process_assets_with_context(&assets, Some(context)).map_err(AssetProcessingFailure::from)
    })
    .map_err(boundary_failure)?
}

/// Process every reachable asset the build collected, the way each really ships.
///
/// Never fails: an asset it cannot process falls back to the raw-byte disclosure that was the whole
/// behaviour before B2, so the result is a strict improvement or a tie, never a regression.
pub fn process_assets(assets: &[CollectedAsset]) -> ProcessedAssets {
    process_assets_with_context(assets, None)
        .expect("unbounded standalone asset processing cannot hit a shared build limit")
}

fn process_assets_with_context(
    assets: &[CollectedAsset],
    context: Option<Arc<AssetProcessingContext>>,
) -> Result<ProcessedAssets, AssetBudgetFailure> {
    let mut processed = ProcessedAssets::default();
    if let Some(failure) = context.as_ref().and_then(|context| context.failure()) {
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
        process_binary_kind(&all_assets, kind, &mut processed, context.as_deref())?;
    }

    // The shared ledger also observes metadata reservations that fail before a provider read and
    // exact snapshots served across retry providers. Merge that whole history on success so a
    // later successful retry cannot erase an earlier conflicting/failed observation from cache
    // freshness merely because both used the same path.
    if let Some(context) = &context {
        processed.read_paths.extend(context.read_paths());
        processed
            .read_time_fingerprints
            .extend(context.freshness_fingerprints());
    }

    processed
        .contributions
        .sort_by_key(|contribution| contribution.kind);
    processed.read_paths.sort();
    processed.read_paths.dedup();
    sort_and_dedup_fingerprints(&mut processed.read_time_fingerprints);
    if let Some(failure) = context.as_ref().and_then(|context| context.failure()) {
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
    context: Option<Arc<AssetProcessingContext>>,
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
    let bundled = bundle_collected_css_set_with_context(&entries, context.clone())
        .and_then(|bundle| compress_bundle(bundle, context.as_deref()))
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
            if context
                .as_ref()
                .and_then(|context| context.failure())
                .is_some()
            {
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
                if context
                    .as_ref()
                    .and_then(|context| context.failure())
                    .is_some()
                {
                    break;
                }
                match bundle_collected_css_with_context(entry, context.clone())
                    .and_then(|bundle| compress_bundle(bundle, context.as_deref()))
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

    if let Some(failure) = context.as_ref().and_then(|context| context.failure()) {
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
                let had_asset_io =
                    !bundle.failed_paths.is_empty() || !bundle.referenced_failures.is_empty();
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
                    processed.failed_paths.push(failure.path.clone());
                    processed.failures.push(failure.message);
                    processed.uncounted.push(UncountedAsset {
                        path: failure.path,
                        bytes: failure.raw_bytes,
                    });
                }
                processed
                    .css_dependency_failures
                    .extend(bundle.dependency_failures);
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
    context: Option<&AssetProcessingContext>,
) -> Result<(CssBundle, CompressionSizes), CssProcessingError> {
    compress_bundle_with(bundle, context, &compress_asset_bytes)
}

fn compress_asset_bytes(bytes: &[u8]) -> Result<CompressionSizes, String> {
    compress_all_bytes(bytes).map_err(|error| error.to_string())
}

fn compress_bundle_with(
    bundle: CssBundle,
    context: Option<&AssetProcessingContext>,
    compress: &dyn Fn(&[u8]) -> Result<CompressionSizes, String>,
) -> Result<(CssBundle, CompressionSizes), CssProcessingError> {
    if let Some(context) = context {
        context.check_deadline().map_err(|error| {
            CssProcessingError::from_transform(error.to_string(), CssReadInputs::default())
        })?;
    }
    match compress(&bundle.minified_bytes) {
        Ok(compressed) => {
            if let Some(context) = context {
                context.check_deadline().map_err(|error| {
                    CssProcessingError::from_transform(error.to_string(), CssReadInputs::default())
                })?;
            }
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
                inputs.failed_paths.push(failure.path);
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
    context: Option<&AssetProcessingContext>,
) -> Result<(), AssetBudgetFailure> {
    process_binary_kind_with(assets, kind, processed, context, &compress_asset_bytes)
}

fn process_binary_kind_with(
    assets: &[CollectedAsset],
    kind: AssetKind,
    processed: &mut ProcessedAssets,
    context: Option<&AssetProcessingContext>,
    compress: &dyn Fn(&[u8]) -> Result<CompressionSizes, String>,
) -> Result<(), AssetBudgetFailure> {
    let mut sizes = MeasuredSizes::ZERO;
    let mut counted = false;

    for asset in assets.iter().filter(|asset| asset.kind == kind) {
        if let Some(context) = context
            && context.check_deadline().is_err()
        {
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
        if let Some(context) = context
            && context.check_deadline().is_err()
        {
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

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("il-assets-{}-{tag}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn css_asset(path: &Path) -> CollectedAsset {
        read_collected_asset(path, AssetKind::Css).expect("stylesheet snapshot")
    }

    #[test]
    fn bundle_css_inlines_the_import_tree_minifies_and_captures_every_read_path() {
        let dir = temp_dir("css");
        let entry = dir.join("index.css");
        let child = dir.join("child.css");
        fs::write(&child, "  .child   {   color :  red ;  }\n").expect("child");
        fs::write(
            &entry,
            "@import \"./child.css\";\n.entry  {  color :  blue ;  }\n",
        )
        .expect("entry");

        let bundle = bundle_css(&entry).expect("a valid stylesheet should bundle");
        fs::remove_dir_all(&dir).ok();

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
        let dir = temp_dir("invalid-utf8-css");
        let child = dir.join("child.css");
        let importing = dir.join("importing.css");
        let top_level = dir.join("top-level.css");
        fs::write(&child, [0xff, 0xfe, 0xfd]).expect("invalid imported stylesheet");
        fs::write(&importing, "@import './child.css';\n.root { color: red; }")
            .expect("importing stylesheet");
        fs::write(&top_level, [0xff, 0xfe, 0xfd]).expect("invalid top-level stylesheet");

        let imported_failure = process_assets(&[css_asset(&importing)]);
        let top_level_failure = process_assets(&[css_asset(&top_level)]);
        fs::remove_dir_all(&dir).ok();

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
        let dir = temp_dir("css-font-url");
        let entry = dir.join("index.css");
        let font = dir.join("probe.woff2");
        fs::write(
            &entry,
            "@font-face { font-family: Probe; src: url('./probe.woff2'); }\n",
        )
        .expect("entry");
        fs::write(&font, [0x5a; 32]).expect("font");

        let expected_font = read_collected_asset(&font, AssetKind::Font).expect("font snapshot");
        let bundle = bundle_css(&entry).expect("a valid stylesheet should bundle");
        fs::remove_dir_all(&dir).ok();

        let css = std::str::from_utf8(&bundle.minified_bytes).expect("utf8");
        assert!(
            css.contains("probe.woff2"),
            "dependency-analysis placeholders must never enter the measured artifact: {css}"
        );
        assert_eq!(bundle.referenced_assets, vec![expected_font]);
        assert!(bundle.dependency_failures.is_empty());
    }

    /// **The daemon-killer.** Lightning CSS cycle-detects on the very path spelling `resolve` hands
    /// back, and the built-in resolver's naive `with_file_name` join never normalizes `..` — so a
    /// cycle crossing a `../` used to yield a longer, distinct key for the same file on every hop,
    /// the dedup never fired, and the recursion overflowed the stack. That is not catchable:
    /// `catch_unwind` never runs, the process `__fastfail`s, and every in-flight request dies with
    /// it. A package can ship such a cycle without knowing — browsers and real bundlers dedupe on a
    /// resolved URL, so it is silent everywhere else.
    ///
    /// If this test ever hangs or aborts the runner rather than failing, THAT is the regression.
    #[test]
    fn a_dot_dot_crossing_import_cycle_terminates_instead_of_killing_the_process() {
        let dir = temp_dir("cycle");
        let components = dir.join("components");
        let theme = dir.join("theme");
        fs::create_dir_all(&components).expect("components");
        fs::create_dir_all(&theme).expect("theme");
        let button = components.join("button.css");
        let tokens = theme.join("tokens.css");
        // A mutual cycle that crosses `../` in both directions.
        fs::write(
            &button,
            "@import \"../theme/tokens.css\";\n.button { color: red }\n",
        )
        .expect("button");
        fs::write(
            &tokens,
            "@import \"../components/button.css\";\n:root { --x: 1 }\n",
        )
        .expect("tokens");
        let other = dir.join("other.css");
        fs::write(&other, ".other { color: teal }\n").expect("other");

        // The multi-entry path is the exposed one: it is what the synthetic entry serves.
        let canonical = |path: &Path| std::fs::canonicalize(path).expect("canonicalize");
        let result = bundle_css_set(&[canonical(&button), canonical(&other)]);
        fs::remove_dir_all(&dir).ok();

        // Terminating at all is the whole assertion: reaching this line means the process survived.
        let bundle = result.expect("a cyclic @import must terminate, not overflow the stack");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");
        // The sheets NOT caught in the cycle are still counted, so one package's broken CSS cannot
        // sink the set. Lightning CSS drops the cyclic sheet's own rules, which undercounts that one
        // stylesheet — pathological input, and still strictly better than before B2, when a
        // CSS-shipping package contributed zero either way. Recorded in known-issues.
        assert!(
            css.contains(".other"),
            "a stylesheet outside the cycle must still be counted: {css}"
        );
    }

    /// The tree is bounded: assets are never graph modules, so none of the engine's limits ever
    /// applied to them. The file count bounds BOTH breadth and depth, because it is the only bound
    /// available: giving the walk its own big stack does NOT work, since Lightning CSS recurses on
    /// rayon workers whose stacks it does not own. 256 is far below where a build's stack gives out
    /// and far above any real stylesheet's `@import` tree.
    #[test]
    fn a_stylesheet_tree_past_the_file_budget_is_refused_rather_than_read_forever() {
        let dir = temp_dir("budget");
        let leaves = MAX_STYLESHEET_FILES + 8;
        let mut entry = String::new();
        for index in 0..leaves {
            fs::write(
                dir.join(format!("leaf{index}.css")),
                format!(".rule{index} {{ color: red }}\n"),
            )
            .expect("leaf");
            entry.push_str(&format!("@import \"./leaf{index}.css\";\n"));
        }
        fs::write(dir.join("index.css"), entry).expect("entry");

        let result = bundle_css(&dir.join("index.css"));
        fs::remove_dir_all(&dir).ok();

        let error = result.expect_err("a tree past the file budget must be refused");
        assert!(error.contains("limit"), "{error}");
    }

    /// An ordinary chain, well inside the budget, still bundles. This is the other half of the bound:
    /// it must refuse the absurd without refusing the real. (It stays shallow deliberately — a test
    /// that recursed near the budget would overflow a DEBUG build's stack, where frames run an order
    /// of magnitude larger than the release build the budget is sized against.)
    #[test]
    fn an_ordinary_import_chain_inside_the_budget_still_bundles() {
        let dir = temp_dir("chain");
        let depth = 24;
        for index in 0..depth {
            let next = if index + 1 < depth {
                format!("@import \"./sheet{}.css\";\n", index + 1)
            } else {
                String::new()
            };
            fs::write(
                dir.join(format!("sheet{index}.css")),
                format!("{next}.rule{index} {{ color: red }}\n"),
            )
            .expect("sheet");
        }

        let result = bundle_css(&dir.join("sheet0.css"));
        fs::remove_dir_all(&dir).ok();

        let bundle = result.expect("an ordinary chain must bundle");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");
        assert!(css.contains(".rule0") && css.contains(".rule23"), "{css}");
    }

    /// A remote `@import` has no file behind it. A real bundler leaves it in the sheet; treating it
    /// as a resolve failure would sink every stylesheet in the set to raw disclosure over a shape
    /// ordinary packages ship.
    #[test]
    fn a_remote_import_is_external_and_does_not_sink_the_stylesheet() {
        let dir = temp_dir("remote");
        let entry = dir.join("index.css");
        fs::write(
            &entry,
            "@import url(\"https://fonts.googleapis.com/css2?family=Inter\");\n.a { color: red }\n",
        )
        .expect("entry");

        let result = bundle_css(&entry);
        fs::remove_dir_all(&dir).ok();

        let bundle = result.expect("a remote @import must not fail the stylesheet");
        let css = std::str::from_utf8(&bundle.minified_bytes).expect("utf8");
        assert!(
            css.contains(".a"),
            "the local rules must still be counted: {css}"
        );
        assert_eq!(
            bundle.dependency_failures.len(),
            1,
            "the unmeasured remote stylesheet must lower confidence: {bundle:?}"
        );
        assert!(
            bundle.dependency_failures[0].contains("fonts.googleapis.com"),
            "the disclosure should identify the external stylesheet: {bundle:?}"
        );
    }

    /// One unprocessable sheet must not take the others down with it. In the File Cost's combined
    /// build the set spans every import in the runtime group, so all-or-nothing meant one package's
    /// `.scss` silently reverted CSS counting for all of them.
    #[test]
    fn one_unparseable_stylesheet_does_not_sink_the_rest_of_the_set() {
        let dir = temp_dir("isolation");
        let good = dir.join("good.css");
        let bad = dir.join("bad.scss");
        fs::write(&good, ".good { color: red }\n").expect("good");
        // Real preprocessor syntax: Lightning CSS parses plain CSS only.
        fs::write(
            &bad,
            "$brand: red;\n@mixin thing { color: $brand }\n.bad { @include thing }\n",
        )
        .expect("bad");
        let assets = vec![css_asset(&good), css_asset(&bad)];
        let expected_bad = assets[1].path.clone();

        let processed = process_assets(&assets);
        fs::remove_dir_all(&dir).ok();

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

    /// When the union AND every per-sheet retry fail, the outcome must be exactly the pre-B2
    /// fallback: each stylesheet disclosed ONCE. The retry used to report each failure as it went and
    /// then hand back an error, so the outer arm disclosed them all a second time and the diagnostic
    /// doubled its own count and byte total — a wrong number in the one place that exists to be
    /// honest about what is missing.
    #[test]
    fn a_set_where_every_stylesheet_fails_discloses_each_of_them_exactly_once() {
        let dir = temp_dir("allfail");
        let first = dir.join("one.scss");
        let second = dir.join("two.scss");
        fs::write(
            &first,
            "$a: red;\n@mixin m { color: $a }\n.x { @include m }\n",
        )
        .expect("one");
        fs::write(
            &second,
            "$b: blue;\n@mixin n { color: $b }\n.y { @include n }\n",
        )
        .expect("two");
        let assets: Vec<CollectedAsset> = [&first, &second]
            .iter()
            .map(|path| css_asset(path))
            .collect();
        let expected_paths = assets
            .iter()
            .map(|asset| asset.path.clone())
            .collect::<Vec<_>>();

        let processed = process_assets(&assets);
        fs::remove_dir_all(&dir).ok();

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

    /// The union can fail for a reason NO individual sheet fails for — it is the only thing charged
    /// for the whole set — and then every sheet counts and `uncounted` is empty. That combination
    /// used to produce a size that was over-counted (a shared `@import` inlined into each sheet),
    /// carried NO diagnostic, and therefore read as High confidence and was written to disk. The
    /// over-count is the accepted cost of degrading; being silent about it was not.
    #[test]
    fn a_set_that_degrades_to_per_sheet_still_discloses_that_it_may_read_high() {
        let dir = temp_dir("degraded");
        // Each sheet's own tree is well inside the budget; only the two together breach it, which
        // is what makes the union fail while each sheet on its own succeeds.
        let per_sheet_leaves = 140;
        let shared = dir.join("shared.css");
        fs::write(&shared, ".shared { color: red }\n").expect("shared");

        let entries: Vec<PathBuf> = ["a", "b"]
            .iter()
            .map(|name| {
                let mut source = String::from("@import \"./shared.css\";\n");
                for index in 0..per_sheet_leaves {
                    let leaf = format!("{name}{index}.css");
                    fs::write(
                        dir.join(&leaf),
                        format!(".r{name}{index} {{ color: red }}\n"),
                    )
                    .expect("leaf");
                    source.push_str(&format!("@import \"./{leaf}\";\n"));
                }
                let entry = dir.join(format!("{name}.css"));
                fs::write(&entry, source).expect("entry");
                entry
            })
            .collect();

        let assets: Vec<CollectedAsset> = entries.iter().map(|path| css_asset(path)).collect();

        // Guard the premise: if this ever stops being the shape under test, the assertions below
        // would pass for the wrong reason.
        assert!(
            bundle_css_set(&entries).is_err(),
            "the premise is a set whose union breaches the budget",
        );
        assert!(
            bundle_css(&entries[0]).is_ok(),
            "the premise is that each sheet on its own is well inside the budget",
        );

        let processed = process_assets(&assets);
        fs::remove_dir_all(&dir).ok();

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
    fn bundle_css_is_an_err_on_a_broken_stylesheet_so_the_caller_can_fall_back() {
        let dir = temp_dir("broken");
        let entry = dir.join("index.css");
        // A dangling @import cannot be resolved from disk: bundling must error, not panic, so the
        // caller reverts to raw-byte disclosure.
        fs::write(
            &entry,
            "@import \"./does-not-exist.css\";\n.a { color: red }\n",
        )
        .expect("entry");

        let result = bundle_css(&entry);
        fs::remove_dir_all(&dir).ok();

        assert!(
            result.is_err(),
            "a dangling @import must be an Err: {result:?}"
        );
    }

    #[test]
    fn a_failed_css_read_remains_unverifiable_after_a_later_success() {
        let dir = temp_dir("failed-then-readable");
        let child = dir.join("created.css");
        let provider = TrackingProvider::new(&[]);

        assert!(provider.read(&child).is_err(), "the first read is missing");
        fs::write(&child, ".created { color: red }").expect("create child");
        assert!(provider.read(&child).is_ok(), "the retry can read it");

        let inputs = provider.into_read_inputs();
        let processed = ProcessedAssets {
            read_paths: inputs.paths,
            read_time_fingerprints: inputs.fingerprints,
            failed_paths: inputs.failed_paths,
            ..ProcessedAssets::default()
        };
        let freshness = processed.freshness_fingerprints();
        fs::remove_dir_all(&dir).ok();

        assert!(
            freshness
                .iter()
                .any(crate::cache::key::fingerprint_is_unverifiable),
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
            dependency_failures: Vec::new(),
        };
        let failure = compress_bundle_with(bundle, None, &|_| Err("injected failure".to_owned()))
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
        let dir = temp_dir("binary-compression-failure");
        let path = dir.join("probe.woff2");
        fs::write(&path, [0x51; 64]).expect("font");
        let asset = read_collected_asset(&path, AssetKind::Font).expect("font snapshot");
        let mut processed = ProcessedAssets::default();

        process_binary_kind_with(&[asset], AssetKind::Font, &mut processed, None, &|_| {
            Err("injected failure".to_owned())
        })
        .expect("a per-asset compressor failure falls back instead of aborting the stage");
        fs::remove_dir_all(&dir).ok();

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
        let dir = temp_dir("set");
        let shared = dir.join("shared.css");
        let first = dir.join("a.css");
        let second = dir.join("b.css");
        fs::write(&shared, ".shared { color: red }\n").expect("shared");
        fs::write(&first, "@import \"./shared.css\";\n.a { color: blue }\n").expect("a");
        fs::write(&second, "@import \"./shared.css\";\n.b { color: green }\n").expect("b");

        let bundle = bundle_css_set(&[first.clone(), second.clone()])
            .expect("both stylesheets should bundle");
        fs::remove_dir_all(&dir).ok();

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
        let dir = temp_dir("process");
        let entry = dir.join("index.css");
        let child = dir.join("child.css");
        fs::write(&child, ".child { color: red }\n").expect("child");
        fs::write(&entry, "@import \"./child.css\";\n.entry { color: blue }\n").expect("entry");

        let processed = process_assets(&[css_asset(&entry)]);
        fs::remove_dir_all(&dir).ok();

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

    /// The fallback that keeps B2 from ever being worse than what it replaced.
    #[test]
    fn process_assets_falls_back_to_raw_disclosure_when_a_stylesheet_cannot_be_processed() {
        let dir = temp_dir("fallback");
        let entry = dir.join("index.css");
        fs::write(&entry, "@import \"./missing.css\";\n.a { color: red }\n").expect("entry");
        let asset = css_asset(&entry);

        let processed = process_assets(std::slice::from_ref(&asset));
        let expected_path = asset.path.clone();
        let expected_bytes = asset.raw_bytes();
        fs::remove_dir_all(&dir).ok();

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
        let dir = temp_dir("binary");
        let wasm = dir.join("engine.wasm");
        let font = dir.join("body.woff2");
        // Deliberately compressible so gzip/brotli/zstd all produce a real number.
        fs::write(&wasm, vec![7_u8; 4096]).expect("wasm");
        fs::write(&font, vec![9_u8; 2048]).expect("font");
        let assets = vec![
            read_collected_asset(&wasm, AssetKind::Wasm).expect("wasm snapshot"),
            read_collected_asset(&font, AssetKind::Font).expect("font snapshot"),
        ];

        let processed = process_assets(&assets);
        fs::remove_dir_all(&dir).ok();

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
