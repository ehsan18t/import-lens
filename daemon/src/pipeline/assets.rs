//! Non-JS asset processing. The engine measures the JS chunk exactly and hands the reachable assets
//! (CSS, wasm, fonts) here to be processed the way they actually ship, so their bytes are folded
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
//! Every path Lightning CSS opens (the entry and each resolved `@import` child) plus supported
//! local artifacts referenced by `url()` are captured for cache freshness, so an edit to any of
//! them invalidates the measured size. Any processing failure falls back to disclosing the raw
//! bytes, never below that floor ([ADR-0006](../../../docs/adr/0006-the-result-model.md)).

use crate::cache::key::{FileFingerprint, sort_and_dedup_fingerprints};
#[cfg(test)]
use crate::engine::read_collected_asset;
use crate::engine::{AssetKind, CollectedAsset, UncountedAsset, diagnostic_stage};
use crate::ipc::protocol::{AssetContribution, ImportDiagnostic, MeasuredSizes};
use crate::pipeline::asset_boundary::{self, AssetBoundaryError, AssetDeadline};
#[cfg(test)]
use crate::pipeline::asset_budget::AssetBudgetLimits;
use crate::pipeline::asset_budget::{AssetBudgetFailure, AssetBudgetStage, AssetProcessingContext};
use crate::pipeline::compress::{CompressionSizes, compress_all_bytes};
use crate::pipeline::css_dependencies::{collect_referenced_assets, is_remote_reference};
use crate::pipeline::css_import_cycles::{self, ImportEdge};
use lightningcss::bundler::{Bundler, FileProvider, ResolveResult, SourceProvider};
use lightningcss::dependencies::DependencyOptions;
use lightningcss::rules::CssRule;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions, StyleSheet};
use lightningcss::targets::Targets;
use oxc_resolver::Resolver;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How many files one `@import` tree may pull in, and how many bytes of them.
///
/// A stylesheet's `@import` children are not graph modules, so [`crate::engine::limits`] does not
/// bound them; this does.
///
/// The file count doubles as the DEPTH bound. Lightning CSS recurses per `@import`, and a deep
/// enough chain overflows the stack (around 800 frames in a release build). That is not catchable:
/// the process `__fastfail`s and the daemon dies with every in-flight request. The canonicalizing
/// `resolve` below breaks cycles; this bounds the honest-but-absurd chain. Refusing at 256 stops
/// the walk roughly three times short of where the release build's stack gives out.
///
/// Breaching either is not a wrong number: the set falls back to the per-sheet path, and failing
/// that to raw-byte disclosure.
///
/// Do not raise it because a flat set of many sheets is harmless: the budget cannot tell breadth
/// from depth, and a bigger stack does not help, because Lightning CSS drives the `@import` graph
/// on `rayon` workers whose stacks it does not own.
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

/// A `SourceProvider` that reads from disk like the built-in `FileProvider`, records every read in
/// the build's ledger, bounds what one `@import` tree may pull in, and can serve one synthetic
/// in-memory entry. The bundler drives the `@import` graph with `rayon`, so the provider must be
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
    budget: Mutex<ReadBudget>,
    /// Every `@import` edge resolved to a file, as (importing sheet, imported sheet).
    edges: Mutex<BTreeSet<ImportEdge>>,
    /// Edges that close a cycle, each answered with its own empty in-memory sheet so the bundle
    /// skips it the way a browser does. Set between the two bundling passes, and only when the
    /// first pass found a cycle.
    cut_edges: Mutex<BTreeMap<ImportEdge, PathBuf>>,
    /// Built on the first bare `@import`; most trees never have one.
    package_resolver: OnceLock<Resolver>,
    /// The ONE read ledger for every union/per-sheet attempt in this build: every path, snapshot
    /// and failed read is recorded there and nowhere else. Never optional: production safety that
    /// a caller can omit is safety the tests will omit.
    context: Arc<AssetProcessingContext>,
}

impl TrackingProvider {
    /// The ONE way to build a provider. Tests pass a context with test limits rather than a
    /// different constructor.
    ///
    /// `preloaded` holds only THIS attempt's entries. Anything an earlier attempt read is looked up
    /// on demand through the context; copying the whole snapshot map per attempt would make each
    /// per-sheet retry pay for every earlier read.
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
            budget: Mutex::new(ReadBudget::default()),
            edges: Mutex::new(BTreeSet::new()),
            cut_edges: Mutex::new(BTreeMap::new()),
            package_resolver: OnceLock::new(),
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

    fn record_failed_read(&self, path: &Path, kind: std::io::ErrorKind) {
        self.context
            .record_failed_path(path, kind == std::io::ErrorKind::NotFound);
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

    /// Where an `@import` lands, without recording the edge.
    fn resolve_target(
        &self,
        specifier: &str,
        originating_file: &Path,
    ) -> Result<ResolveResult, std::io::Error> {
        // A REMOTE `@import` (`@import url("https://fonts.googleapis.com/…")`, or any other scheme
        // such as `data:`) has no file behind it; a real bundler leaves it in the sheet, and so do
        // we. Treating it as a resolve failure would sink the whole set to raw disclosure.
        if is_remote_reference(specifier) {
            return Ok(ResolveResult::External(specifier.to_owned()));
        }

        // The synthetic entry `@import`s absolute paths; resolve those directly. `FileProvider`'s
        // own resolve is a naive relative join and would mangle them.
        let candidate = Path::new(specifier);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            match self.inner.resolve(specifier, originating_file)? {
                ResolveResult::File(path) if is_bare_specifier(specifier) => {
                    self.bare_import_target(specifier, originating_file, path)
                }
                ResolveResult::File(path) => path,
                external @ ResolveResult::External(_) => return Ok(external),
            }
        };

        // CANONICALIZE: Lightning CSS cycle-detects on the PathBuf spelling this returns, and
        // `FileProvider::resolve` never normalizes `..`. A cycle crossing `../` would hand back
        // a longer, distinct key for the same file on every hop and overflow the stack, which
        // `catch_unwind` cannot catch, killing the daemon. Browsers and real bundlers tolerate
        // `@import` cycles, so packages can ship one unknowingly. A canonical key terminates it.
        Ok(ResolveResult::File(
            std::fs::canonicalize(&resolved).unwrap_or(resolved),
        ))
    }

    /// A bare `@import "pkg/base.css"` is a URL relative to the sheet first, as CSS defines it,
    /// and names a package only when no such file exists, which is how esbuild, Vite and
    /// postcss-import read it. A package entry that is not a stylesheet is left unresolved rather
    /// than measured as CSS.
    fn bare_import_target(
        &self,
        specifier: &str,
        originating_file: &Path,
        relative: PathBuf,
    ) -> PathBuf {
        match std::fs::metadata(&relative) {
            Ok(metadata) if metadata.is_file() => return relative,
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return relative,
            _ => {}
        }
        let Some(directory) = originating_file.parent() else {
            return relative;
        };
        let resolver = self
            .package_resolver
            .get_or_init(|| Resolver::new(crate::pipeline::resolver::stylesheet_resolve_options()));
        match resolver.resolve(directory, specifier) {
            Ok(resolution) if is_stylesheet_path(resolution.path()) => {
                // A file appearing at the relative spelling would take precedence, so its absence
                // is part of what this result was measured from.
                self.record_failed_read(&relative, std::io::ErrorKind::NotFound);
                resolution.into_path_buf()
            }
            _ => relative,
        }
    }

    /// The sheet's `@import` targets in source order. Only the cycle-cutting pass reads it, so a
    /// sheet is parsed a second time only in a tree that has a cycle.
    fn ordered_imports(&self, sheet: &Path) -> Vec<PathBuf> {
        let bytes = match &self.synthetic {
            Some((path, content)) if path == sheet => content.clone().into_bytes(),
            _ => match self
                .preloaded
                .get(sheet)
                .cloned()
                .or_else(|| self.context.snapshot_for(sheet))
            {
                Some(asset) => asset.bytes().to_vec(),
                None => return Vec::new(),
            },
        };
        let Ok(source) = std::str::from_utf8(&bytes) else {
            return Vec::new();
        };
        let Ok(stylesheet) = StyleSheet::parse(source, ParserOptions::default()) else {
            return Vec::new();
        };
        stylesheet
            .rules
            .0
            .iter()
            .filter_map(|rule| match rule {
                CssRule::Import(import) => match self.resolve_target(&import.url, sheet) {
                    Ok(ResolveResult::File(target)) => Some(target),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The edges to cut so the tree bundles as a browser applies it, or `None` for an acyclic tree.
    fn closing_edges(&self, entry: &Path) -> Option<BTreeSet<ImportEdge>> {
        let recorded = css_import_cycles::sorted_adjacency(
            &self
                .edges
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let any_order = |sheet: &Path| recorded.get(sheet).cloned().unwrap_or_default();
        if css_import_cycles::closing_edges(entry, &any_order).is_empty() {
            return None;
        }
        Some(css_import_cycles::closing_edges(entry, &|sheet| {
            self.ordered_imports(sheet)
        }))
    }

    /// Answer every closing edge with an empty sheet, and give the second pass a fresh per-tree
    /// budget: it walks the same tree again, not a bigger one.
    fn cut(&self, closing: BTreeSet<ImportEdge>) {
        let mut cut_edges = self
            .cut_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (index, edge) in closing.into_iter().enumerate() {
            let empty = edge
                .0
                .with_file_name(format!("__import_lens_cut_import_{index}__.css"));
            cut_edges.insert(edge, empty);
        }
        *self
            .budget
            .lock()
            .expect("css read budget should not be poisoned") = ReadBudget::default();
    }

    fn is_cut_sheet(&self, file: &Path) -> bool {
        self.cut_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|empty| empty == file)
    }
}

/// Neither `./`, `../` nor rooted: the spelling that may name a package.
fn is_bare_specifier(specifier: &str) -> bool {
    !(specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/'))
}

fn is_stylesheet_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("css"))
}

impl SourceProvider for TrackingProvider {
    type Error = std::io::Error;

    fn read<'a>(&'a self, file: &Path) -> Result<&'a str, Self::Error> {
        self.check_deadline()?;
        // The synthetic entry has no file behind it, so it is served from memory and never recorded
        // as a freshness input.
        if let Some((path, content)) = &self.synthetic
            && file == path
        {
            return Ok(content.as_str());
        }
        // A cut edge's stand-in is empty and has no file behind it either.
        if self.is_cut_sheet(file) {
            return Ok("");
        }

        // Canonicalize so a cache key is stable across `..` / symlink spellings of the same file.
        let key = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        // Every outcome below reaches the ledger: a snapshot charge, a metadata reservation, or a
        // failed read. A missing or broken child must stay in freshness even with no bytes to hash.
        //
        // This attempt's own entry first, then anything an earlier attempt already read. Reusing the
        // ledger's snapshot keeps a retry measuring the SAME bytes the union measured, and stops it
        // charging the same file twice.
        let snapshot = self
            .preloaded
            .get(&key)
            .cloned()
            .or_else(|| self.context.snapshot_for(&key));
        let source = match snapshot {
            Some(asset) => {
                self.context.charge_css_snapshot(&asset)?;
                self.reserve(asset.bytes().len())?;
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
                        self.record_failed_read(&key, error.kind());
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
                        self.record_failed_read(&key, error.kind());
                        return Err(error);
                    }
                };
                self.reconcile(reservation, bytes.len())?;
                self.context.finish_css_read(shared_reservation, &bytes)?;
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
        let target = match self.resolve_target(specifier, originating_file)? {
            ResolveResult::File(target) => target,
            external @ ResolveResult::External(_) => return Ok(external),
        };
        let edge = (originating_file.to_path_buf(), target);
        if let Some(empty) = self
            .cut_edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&edge)
        {
            return Ok(ResolveResult::File(empty.clone()));
        }
        let target = edge.1.clone();
        self.edges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(edge);
        Ok(ResolveResult::File(target))
    }
}

/// A reachable stylesheet set processed as it ships: the bundled bytes before and after
/// minification, plus every file that fed them (for freshness).
#[derive(Debug)]
pub struct CssBundle {
    /// The `@import`-inlined stylesheet before minification, mirroring the JS chunk's `raw_bytes`.
    pub raw_bytes: Vec<u8>,
    /// The `@import`-inlined, minified stylesheet: what actually ships, and what gets compressed.
    pub minified_bytes: Vec<u8>,
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
    /// Dependency analysis must never discard an otherwise valid CSS size, so the CSS is kept and
    /// the omission disclosed. These bytes are missing from the total, so the result is a floor.
    pub dependency_omissions: Vec<String>,
    /// Runtime-fetched resources: real weight, but not bytes this package ships, so the measured
    /// size is exact and stays budgetable.
    pub dependency_external: Vec<String>,
}

/// Why a stylesheet attempt failed. What it read is in the build's ledger, not here: a read failure
/// that makes the result request-local is derived from the ledger's sentinels (see
/// [`asset_io_diagnostic`]).
#[derive(Debug)]
struct CssProcessingError {
    message: String,
    non_durable_stages: BTreeSet<&'static str>,
}

impl CssProcessingError {
    fn from_transform(message: String) -> Self {
        Self {
            message,
            non_durable_stages: BTreeSet::new(),
        }
    }

    fn from_compression(message: String) -> Self {
        Self {
            message,
            non_durable_stages: BTreeSet::from([crate::pipeline::stage::COMPRESSION]),
        }
    }
}

/// Bundle one stylesheet the way it ships: resolve its `@import` tree from disk into one
/// stylesheet, minify with deterministic (target-free) output, and print. Any failure is an `Err`;
/// the caller falls back to raw-byte disclosure.
///
/// Test-only, and it builds a REAL ledger rather than skipping one.
#[cfg(test)]
pub fn bundle_css(entry: &Path) -> Result<CssBundle, String> {
    let asset = read_collected_asset(entry, AssetKind::Css)
        .map_err(|error| format!("failed to read stylesheet {}: {error}", entry.display()))?;
    let context = test_context(std::slice::from_ref(&asset));
    bundle_collected_css(&asset, context).map_err(|error| error.message)
}

/// A production-shaped ledger for a test that only wants to bundle something.
///
/// Production limits deliberately: a test under looser bounds would measure a different system.
/// The per-attempt stylesheet-tree bound lives on the provider and still applies.
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
    bundle_with(&provider, &entry.path).map_err(CssProcessingError::from_transform)
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
        [] => Err(CssProcessingError::from_transform(
            "no stylesheets to bundle".to_owned(),
        )),
        [single] => bundle_collected_css(single, context),
        many => {
            let paths = many
                .iter()
                .map(|asset| asset.path.clone())
                .collect::<Vec<_>>();
            let (path, content) = synthetic_entry(&paths);
            let provider = TrackingProvider::new(many, Some((path.clone(), content)), context);
            bundle_with(&provider, &path).map_err(CssProcessingError::from_transform)
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
/// Backslash and double-quote are the only characters that can end the string or start an escape,
/// so escaping them is the whole job. A package may ship a file whose name contains a quote (POSIX
/// allows it), which would otherwise inject rules into the sheet being measured.
///
/// Never rewrite `\` to `/`: that corrupts a POSIX path containing a literal backslash, and turns a
/// Windows verbatim `\\?\C:\…` prefix into a non-verbatim `//?/C:/…`, switching off the `..`
/// normalization that keeps an `@import` cycle from recursing forever.
fn css_string_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The provider must outlive the `StyleSheet` (it borrows source strings held inside it), so all
/// consumption happens here and only owned bytes escape.
fn bundle_with(provider: &TrackingProvider, entry: &Path) -> Result<CssBundle, String> {
    provider
        .check_deadline()
        .map_err(|error| error.to_string())?;
    let bundle = || {
        Bundler::new(provider, None, ParserOptions::default())
            .bundle(entry)
            .map_err(|error| {
                format!(
                    "lightningcss failed to bundle {}: {error:?}",
                    entry.display()
                )
            })
    };
    let mut stylesheet = bundle()?;
    if let Some(closing) = provider.closing_edges(entry) {
        drop(stylesheet);
        provider.cut(closing);
        provider
            .check_deadline()
            .map_err(|error| error.to_string())?;
        stylesheet = bundle()?;
    }
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
                &|path| provider.context.observe_metadata(path),
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
    /// The build ledger's observations: fingerprints captured by the same reads that supplied every
    /// measured asset byte, plus a sentinel for every failed read. A later success in the same
    /// union/retry flow does not erase a failure: that mixed observation cannot be reused.
    pub read_time_fingerprints: Vec<FileFingerprint>,
    /// Assets that could NOT be processed, disclosed with their raw bytes.
    pub uncounted: Vec<UncountedAsset>,
    /// Why each of those fell back, for the diagnostic.
    pub failures: Vec<String>,
    /// Why the stylesheet set could not be bundled as ONE artifact, when it could not.
    ///
    /// Every sheet is still counted, so this leaves `uncounted` EMPTY. That is why it is its own
    /// field: `failures` is read only as the detail of the `uncounted` disclosure, which is silent
    /// when nothing is uncounted. Every channel here has one consumer and its own trigger.
    pub stylesheets_measured_separately: Option<String>,
    /// Local resources a counted stylesheet references that are missing from the total and whose
    /// size is not even known: an unlocatable path, an unreadable file, or a sheet whose URLs could
    /// not be inspected at all.
    ///
    /// This is an OMISSION channel (the number is a floor), never `imprecise_assets` (which means
    /// the number reads HIGH).
    pub css_dependency_omissions: Vec<String>,
    /// The `uncounted` rows are a lower bound on what is missing: a resource limit stopped the
    /// walk, so the `@import` children and `url()` resources those rows reach are missing too.
    pub uncounted_total_is_floor: bool,
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

    /// Whether supported asset bytes are disclosed but absent from [`Self::total`]. A deterministic
    /// processor rejection is reusable at the import level, but any aggregate containing it is a
    /// lower bound rather than a complete File Cost.
    pub fn has_uncounted_assets(&self) -> bool {
        // A CSS-referenced omission counts here even though it has no `UncountedAsset` row: the
        // bytes are missing from the total just the same, and an unknown size makes the result MORE
        // of a floor, not less.
        !self.uncounted.is_empty() || !self.css_dependency_omissions.is_empty()
    }

    /// Every observation the build's ledger made: exact snapshots, stat-only observations, and a
    /// sentinel for each failed read (absent while a missing file stays missing, unverifiable for
    /// any other failure). Both Import Cost and File Cost consume this one set.
    pub fn freshness_fingerprints(&self) -> Vec<FileFingerprint> {
        let mut fingerprints = self.read_time_fingerprints.clone();
        sort_and_dedup_fingerprints(&mut fingerprints);
        fingerprints
    }

    /// Inputs whose read failed for a reason other than absence: a moment on this machine, which
    /// makes the result request-local.
    fn unreadable_inputs(&self) -> Vec<String> {
        let mut paths = self
            .read_time_fingerprints
            .iter()
            .filter(|fingerprint| crate::cache::key::fingerprint_is_unverifiable(fingerprint))
            .map(|fingerprint| fingerprint.path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// The disclosure for assets that could NOT be processed: their bytes are real, they ship, and they
/// are not in the number. `None` when everything was counted (the normal case), and that absence is
/// what lets a CSS-shipping package leave Medium confidence.
pub fn uncounted_assets_diagnostic(processed: &ProcessedAssets) -> Option<ImportDiagnostic> {
    if processed.uncounted.is_empty() {
        return None;
    }

    Some(ImportDiagnostic {
        stage: diagnostic_stage::UNCOUNTED_ASSETS.to_owned(),
        message: crate::engine::uncounted_assets_message(
            &processed.uncounted,
            processed.uncounted_total_is_floor,
        ),
        details: processed.failures.clone(),
    })
}

/// The disclosure for assets that ARE counted but whose bytes are counted more than once.
///
/// The union buys TWO things: it dedupes an `@import` two sheets share, AND it puts the whole set
/// through ONE compression stream. When it fails, each sheet is measured and compressed alone. The
/// second term dominates: 300 tiny sheets sharing no `@import` at all (the shape that actually
/// breaches the file budget) sum to ~40x the union's gzip and ~57x its brotli, because every stream
/// restarts its window and pays its own header.
///
/// So this fires on the union having failed, not on the sheets provably sharing bytes: sheets that
/// share nothing are the worst case, not the safe one. `None` when the union held.
///
/// Separate from [`uncounted_assets_diagnostic`], which reports bytes missing (and returns early
/// when nothing is uncounted, exactly the degraded case); this reports bytes over-counted.
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
/// `UNCOUNTED_ASSETS`, not `IMPRECISE_ASSETS`: imprecise means the number reads HIGH, uncounted
/// means bytes are missing and the number is a floor, which is what makes `incomplete` fire.
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
/// a CDN font is weight the page pays but not weight this package carries. Never route these
/// through a precision stage, which would refuse a budget verdict. The disclosure still costs High
/// confidence: there is runtime weight this number does not model.
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
    let paths = processed.unreadable_inputs();
    if paths.is_empty() {
        return None;
    }
    Some(ImportDiagnostic {
        stage: crate::engine::stage::ASSET_IO.to_owned(),
        message: "one or more asset inputs could not be read during this analysis; the result \
                  reflects a changing or unavailable filesystem and will not be reused"
            .to_owned(),
        details: paths
            .iter()
            .map(|path| format!("unreadable asset input: {path}"))
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
/// Public because the freshness integration test measures through it; there is no unbounded entry.
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

/// A resource-ledger breach as a DISCLOSURE rather than an error: every collected asset at its raw
/// size, none counted, with the observations that expire it.
///
/// The import's JavaScript is measured before this stage runs, so failing the stage would discard a
/// complete measurement and report the import Unmeasured, below the floor FR-018a promises. The
/// number stands and says what it could not reach.
fn disclose_budget_breach(
    assets: &[CollectedAsset],
    failure: AssetBudgetFailure,
) -> ProcessedAssets {
    let mut processed = ProcessedAssets {
        read_paths: failure.read_paths,
        read_time_fingerprints: failure.read_time_fingerprints,
        failures: vec![failure.message],
        uncounted_total_is_floor: true,
        uncounted: assets
            .iter()
            .map(|asset| UncountedAsset {
                path: asset.path.clone(),
                bytes: asset.raw_bytes(),
            })
            .collect(),
        ..ProcessedAssets::default()
    };
    if processed.uncounted.is_empty() {
        // Nothing sizeable to disclose, but bytes were still left unread. Without this the result
        // would read complete, which is the one thing a breach must never let it do.
        processed.css_dependency_omissions.push(
            "asset processing stopped at its resource limit, so bytes it had not reached are not \
             in this size"
                .to_owned(),
        );
    }
    processed
}

/// Process every reachable asset the build collected, the way each really ships.
///
/// Never fails on an asset it cannot process: that falls back to raw-byte disclosure. A breach of
/// the shared ledger, whenever it is detected, is a deterministic fact about the build and becomes
/// [`disclose_budget_breach`]'s floor. Only the deadline fails the stage, because a timeout is
/// request-local and must never be cached.
fn process_assets(
    assets: &[CollectedAsset],
    context: Arc<AssetProcessingContext>,
) -> Result<ProcessedAssets, AssetBudgetFailure> {
    let mut processed = ProcessedAssets::default();
    if context.failure().is_none() {
        count_assets(assets, &mut processed, &context);
    }
    match context.failure() {
        None => {}
        Some(failure) if failure.stage == AssetBudgetStage::Timeout => return Err(failure),
        Some(failure) => return Ok(disclose_budget_breach(assets, failure)),
    }

    // Freshness is the ledger's whole history, so a later successful retry cannot erase an earlier
    // conflicting or failed observation of the same path.
    processed.read_paths = context.read_paths();
    processed.read_time_fingerprints = context.freshness_fingerprints();
    processed
        .contributions
        .sort_by_key(|contribution| contribution.kind);
    Ok(processed)
}

/// Count every asset into `processed`, stopping as soon as the shared ledger records a failure.
/// Partial work is left for [`process_assets`] to discard.
fn count_assets(
    assets: &[CollectedAsset],
    processed: &mut ProcessedAssets,
    context: &Arc<AssetProcessingContext>,
) {
    let referenced_assets = process_stylesheets(assets, processed, context.clone());
    if context.failure().is_some() {
        return;
    }
    let mut assets_by_path: BTreeMap<PathBuf, CollectedAsset> = assets
        .iter()
        .cloned()
        .map(|asset| (asset.path.clone(), asset))
        .collect();
    // One emitted file per path. Every observation of it is already in the ledger, so a resource
    // that changed between a direct graph load and CSS dependency analysis still leaves two
    // conflicting fingerprints behind and the run is not reused.
    for asset in referenced_assets {
        assets_by_path.entry(asset.path.clone()).or_insert(asset);
    }
    let all_assets: Vec<CollectedAsset> = assets_by_path.into_values().collect();

    for kind in AssetKind::BINARY {
        process_binary_kind(&all_assets, kind, processed, context);
    }
}

/// The settled result of bundling the stylesheet set, named rather than a bare tuple so that the
/// degraded flag cannot be dropped on its way out of the retry.
struct StylesheetOutcome {
    counted: Vec<(CssBundle, CompressionSizes)>,
    /// Why individual sheets fell back, one per entry in `uncounted`.
    failures: Vec<String>,
    uncounted: Vec<UncountedAsset>,
    /// A request-local cause stays sticky across the union/per-sheet retry. A later success must
    /// not turn an earlier compressor failure into a reusable package fact.
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
) -> Vec<CollectedAsset> {
    let entries: Vec<CollectedAsset> = assets
        .iter()
        .filter(|asset| asset.kind == AssetKind::Css)
        .cloned()
        .collect();
    if entries.is_empty() {
        return Vec::new();
    }

    // One artifact for the whole set is right (it is how CSS ships, and it dedupes what two sheets
    // share), but a union fails as a unit: one `.scss`, or one unresolvable `@import`, would take
    // down every stylesheet in the runtime group. So if the union fails, retry per sheet: the ones
    // that parse are still counted and only the offender falls back, at the cost of no dedup.
    //
    // NOTHING is written to `processed` until the outcome is settled, so the all-fail path
    // discloses each stylesheet exactly once.
    //
    // The degradation is its own state, NOT a line in `failures`: `failures` only details the
    // uncounted disclosure, which is silent when every sheet counts (the common outcome, since the
    // union usually fails on a budget each sheet is well inside).
    let bundled = bundle_collected_css_set(&entries, context.clone())
        .and_then(|bundle| compress_bundle(bundle, &context))
        .map(|counted| StylesheetOutcome {
            counted: vec![counted],
            failures: Vec::new(),
            uncounted: Vec::new(),
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
                        non_durable_stages.extend(error.non_durable_stages);
                        uncounted.push(UncountedAsset {
                            path: entry.path.clone(),
                            bytes: entry.raw_bytes(),
                        });
                    }
                }
            }

            // Every sheet failed: hand back the union's error and let the one disclosure below
            // cover each sheet exactly once.
            if counted.is_empty() {
                return Err(CssProcessingError {
                    message: union_message,
                    non_durable_stages,
                });
            }
            Ok(StylesheetOutcome {
                counted,
                failures,
                uncounted,
                non_durable_stages,
                // Sheets DID count here, so `uncounted` may be empty and its disclosure silent;
                // this is what discloses the over-count.
                degraded: Some(union_message),
            })
        });

    // A ledger failure settles the whole stage; `process_assets` owns its one disclosure.
    if context.failure().is_some() {
        return Vec::new();
    }

    let mut referenced_assets = Vec::new();
    match bundled {
        Ok(StylesheetOutcome {
            counted,
            failures,
            uncounted,
            non_durable_stages,
            degraded,
        }) => {
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
                css.raw_bytes += bundle.raw_bytes.len() as u64;
                css.minified_bytes += bundle.minified_bytes.len() as u64;
                css.gzip_bytes += compressed.gzip_bytes;
                css.brotli_bytes += compressed.brotli_bytes;
                css.zstd_bytes += compressed.zstd_bytes;
                referenced_assets.extend(bundle.referenced_assets);
                // The failed read itself is already in the ledger, under the sentinel its reason
                // earns; only the disclosure is added here.
                for failure in bundle.referenced_failures {
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

    referenced_assets
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
    let deadline_error =
        |error: std::io::Error| CssProcessingError::from_transform(error.to_string());
    context.check_deadline().map_err(deadline_error)?;
    match compress(&bundle.minified_bytes) {
        Ok(compressed) => {
            context.check_deadline().map_err(deadline_error)?;
            Ok((bundle, compressed))
        }
        Err(error) => Err(CssProcessingError::from_compression(format!(
            "failed to compress the bundled stylesheet: {error}"
        ))),
    }
}

fn process_binary_kind(
    assets: &[CollectedAsset],
    kind: AssetKind,
    processed: &mut ProcessedAssets,
    context: &AssetProcessingContext,
) {
    process_binary_kind_with(assets, kind, processed, context, &compress_asset_bytes);
}

/// An expired deadline stops the loop; the context retains the typed failure for
/// [`process_assets`] to settle.
fn process_binary_kind_with(
    assets: &[CollectedAsset],
    kind: AssetKind,
    processed: &mut ProcessedAssets,
    context: &AssetProcessingContext,
    compress: &dyn Fn(&[u8]) -> Result<CompressionSizes, String>,
) {
    let mut sizes = MeasuredSizes::ZERO;
    let mut counted = false;

    for asset in assets.iter().filter(|asset| asset.kind == kind) {
        if context.check_deadline().is_err() {
            return;
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
            return;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A temp workspace that writes its files up front and deletes itself on drop, so an assertion
    /// failing mid-test cannot leak the directory.
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
        let processed = process_assets_for_test(&[css_asset(&fixture.path("index.css"))]);
        for name in ["index.css", "child.css"] {
            assert!(
                processed.read_paths.iter().any(|path| path.ends_with(name)),
                "{name} must be captured: {:?}",
                processed.read_paths,
            );
        }
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

    /// `@import "theme.css"` (no `./`) is a RELATIVE url in CSS, and one of the most common shapes
    /// real stylesheets ship. Deciding by spelling alone would drop every such sheet to raw-byte
    /// disclosure; the file's existence beside the sheet is what decides.
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
    /// so a non-canonical key for a cycle crossing `../` would differ on every hop and overflow the
    /// stack (uncatchable `__fastfail`, killing every in-flight request).
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
        assert!(
            css.contains(".other"),
            "a stylesheet outside the cycle must still be counted: {css}"
        );
        // A browser skips only the `@import` that closes the cycle, so each sheet's own rules ship.
        assert_eq!(css.matches(".button").count(), 1, "{css}");
        assert_eq!(css.matches("--x").count(), 1, "{css}");
    }

    /// A browser skips the `@import` that closes a cycle and applies every sheet's own rules once, so
    /// both sheets of a mutual cycle are counted, each once.
    #[test]
    fn every_sheet_of_an_import_cycle_keeps_its_own_rules() {
        let fixture = Fixture::new(
            "cycle-rules",
            &[
                (
                    "button.css",
                    "@import \"./tokens.css\";\n.button { color: red }\n",
                ),
                (
                    "tokens.css",
                    "@import \"./button.css\";\n.tokens { color: blue }\n",
                ),
            ],
        );

        let bundle = bundle_css(&fixture.path("button.css")).expect("a cycle must bundle");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");

        assert_eq!(css.matches(".button").count(), 1, "{css}");
        assert_eq!(css.matches(".tokens").count(), 1, "{css}");
    }

    /// A bare `@import` names a package, the way Vite and postcss-import read it once the path
    /// is not a file next to the sheet: its `style` field or condition, else its CSS entry.
    #[test]
    fn a_bare_import_resolves_into_the_named_package() {
        let fixture = Fixture::new(
            "bare-import",
            &[
                (
                    "node_modules/theme-kit/package.json",
                    r#"{"name":"theme-kit","version":"1.0.0","style":"dist/theme.css"}"#,
                ),
                (
                    "node_modules/theme-kit/dist/theme.css",
                    ".theme-kit-root { color: green }\n",
                ),
                (
                    "node_modules/theme-kit/base.css",
                    ".theme-kit-base { margin: 0 }\n",
                ),
                (
                    "node_modules/widget/index.css",
                    "@import \"theme-kit\";\n@import \"theme-kit/base.css\";\n.widget { color: red }\n",
                ),
            ],
        );

        let bundle = bundle_css(&fixture.path("node_modules/widget/index.css"))
            .expect("a bare import must bundle");
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");

        assert!(css.contains(".theme-kit-root"), "{css}");
        assert!(css.contains(".theme-kit-base"), "{css}");
        assert!(css.contains(".widget"), "{css}");
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
        // Name the bound: the build-wide CSS work ledger can refuse the same tree with a message
        // that also contains "limit", so asserting on that word alone cannot tell which mechanism
        // stopped the walk.
        assert!(
            error.contains("stylesheet @import tree exceeds"),
            "the per-attempt file bound must be what refused this tree: {error}"
        );
    }

    /// The other half of the bound: it must refuse the absurd without refusing the real. It stays
    /// shallow deliberately: recursing near the budget would overflow a DEBUG build's stack, whose
    /// frames run an order of magnitude larger than the release build it is sized for.
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

    /// A protocol-relative `url()` names a CDN, exactly like the `https://` form, not an
    /// unlocatable local file that would mark the size a floor.
    #[test]
    fn a_protocol_relative_url_is_external_rather_than_an_unlocatable_local_file() {
        let fixture = Fixture::new(
            "protocol-relative",
            &[(
                "index.css",
                "@font-face { src: url(\"//fonts.gstatic.com/s/roboto/v30/x.woff2\") }\n",
            )],
        );

        let bundle = bundle_css(&fixture.path("index.css")).expect("the sheet must still bundle");
        assert_eq!(
            bundle.dependency_external.len(),
            1,
            "a protocol-relative resource is fetched at runtime: {bundle:?}"
        );
        assert!(
            bundle.dependency_omissions.is_empty(),
            "an exact measurement must not be turned into a floor: {bundle:?}"
        );

        let mut processed = ProcessedAssets::default();
        processed
            .css_dependency_external
            .extend(bundle.dependency_external);
        processed
            .css_dependency_omissions
            .extend(bundle.dependency_omissions);
        assert!(
            !processed.has_uncounted_assets(),
            "runtime-fetched weight must keep the size exact and budgetable"
        );
    }

    /// Percent-escapes that do not decode to UTF-8 (a CP-1252 export) still name a shipped file, so
    /// the reference is disclosed as an omission. Nothing may leave without a trace.
    #[test]
    fn a_url_whose_escapes_are_not_utf8_is_named_rather_than_dropped() {
        let fixture = Fixture::new(
            "undecodable-url",
            &[(
                "index.css",
                "@font-face { src: url(\"Ubuntu-R%E9gular.woff2\") }\n",
            )],
        );

        let bundle = bundle_css(&fixture.path("index.css")).expect("the sheet must still bundle");
        assert_eq!(
            bundle.dependency_omissions.len(),
            1,
            "an uninterpretable reference is an omission, never a silent drop: {bundle:?}"
        );
        assert!(
            bundle.dependency_omissions[0].contains("Ubuntu-R%E9gular.woff2"),
            "the disclosure must name the reference it could not interpret: {bundle:?}"
        );

        let mut processed = ProcessedAssets::default();
        processed
            .css_dependency_omissions
            .extend(bundle.dependency_omissions);
        assert!(
            processed.has_uncounted_assets(),
            "missing bytes of unknown size make the result a floor"
        );
        assert!(
            uncounted_assets_diagnostic(&processed).is_some()
                || omitted_css_resources_diagnostic(&processed).is_some(),
            "the floor must be disclosed to the user: {processed:?}"
        );
    }

    /// A `url()` target that is simply absent is a deterministic fact about the package, not a fact
    /// about this machine, so it must not be recorded as a transient (`asset_io`) failure.
    #[test]
    fn a_missing_url_target_is_recorded_as_absent_not_as_transient_io() {
        let fixture = Fixture::new(
            "missing-url-target",
            &[(
                "index.css",
                "@font-face { src: url(\"./missing.woff2\") }\n",
            )],
        );

        let bundle = bundle_css(&fixture.path("index.css")).expect("the sheet must still bundle");
        assert_eq!(
            bundle.referenced_failures.len(),
            1,
            "the unreadable target must be recorded: {bundle:?}"
        );

        // Absent, not unverifiable: fresh only while the file stays missing, stale the moment it
        // appears, and never the request-local `asset_io`.
        let processed = process_assets_for_test(&[css_asset(&fixture.path("index.css"))]);
        let fingerprints = processed.freshness_fingerprints();
        assert!(
            fingerprints.contains(&crate::cache::key::absent_file_fingerprint(
                bundle.referenced_failures[0].path.clone()
            )),
            "a missing target takes the absent-state sentinel: {fingerprints:?}"
        );
        assert!(
            crate::cache::key::fingerprints_are_reusable(&fingerprints),
            "an absence must not refuse the cache: {fingerprints:?}"
        );
        assert!(
            asset_io_diagnostic(&processed).is_none(),
            "an absence is not a filesystem incident: {processed:?}"
        );
    }

    /// `@import` and `url()` share one remote predicate, so an `@import` with any URL scheme is left
    /// in the sheet rather than joined onto the sheet's directory as a file that cannot be read.
    #[test]
    fn a_data_import_is_external_and_does_not_sink_the_stylesheet() {
        let fixture = Fixture::new(
            "data-import",
            &[(
                "index.css",
                "@import url(\"data:text/css,.inline{color:red}\");\n.a { color: blue }\n",
            )],
        );
        let assets = vec![css_asset(&fixture.path("index.css"))];

        let processed = process_assets_for_test(&assets);

        assert!(
            processed.uncounted.is_empty() && asset_diagnostics(&processed).is_empty(),
            "a data: @import names no file: {processed:?}"
        );
        assert!(
            processed
                .contributions
                .iter()
                .any(|contribution| contribution.kind == AssetKind::Css),
            "the stylesheet must be counted: {processed:?}"
        );
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
        // measured size is EXACT and must keep its budget verdict.
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

    /// An image a counted stylesheet references ships as its own file, so it is counted at its real
    /// size, like a font.
    #[test]
    fn an_image_referenced_by_css_is_counted_at_its_real_size() {
        let fixture = Fixture::new(
            "image-url",
            &[("index.css", ".a { background-image: url('./bg.png') }\n")],
        );
        fixture.write_bytes("bg.png", &[7u8; 4096]);

        let bundle = bundle_css(&fixture.path("index.css"))
            .expect("an image reference must not fail the stylesheet");
        assert_eq!(
            bundle
                .referenced_assets
                .iter()
                .map(|asset| (asset.kind, asset.raw_bytes()))
                .collect::<Vec<_>>(),
            vec![(AssetKind::Image, 4096)],
            "{bundle:?}"
        );
        assert!(bundle.referenced_uncounted.is_empty(), "{bundle:?}");
    }

    /// The headline claims to be the import's full cost, so a shipped file it does not include has
    /// to say so: a media file outside the counted taxonomy is disclosed, never silently dropped.
    #[test]
    fn a_media_file_referenced_by_css_is_disclosed_with_its_real_size() {
        let fixture = Fixture::new(
            "media-url",
            &[("index.css", ".a { background-image: url('./clip.mp4') }\n")],
        );
        fixture.write_bytes("clip.mp4", &[7u8; 4096]);

        let bundle = bundle_css(&fixture.path("index.css"))
            .expect("a media reference must not fail the stylesheet");
        assert_eq!(
            bundle.referenced_uncounted.len(),
            1,
            "the shipped media file must be disclosed: {bundle:?}"
        );
        assert_eq!(
            bundle.referenced_uncounted[0].bytes, 4096,
            "the disclosure must carry the file's real size, not a zero: {bundle:?}"
        );
        assert!(
            bundle.dependency_omissions.is_empty(),
            "a resolvable media file is a sized disclosure, not an unknown omission: {bundle:?}"
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

    /// One unprocessable sheet must not take the others down with it: the set spans every import in
    /// the runtime group.
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

    /// A resource-ledger breach must not cost the import its JavaScript.
    ///
    /// That measurement is already complete when this stage runs, and a breach is durable, so
    /// failing the stage would cache Unmeasured for a package whose code measured perfectly. The
    /// number stands and says what is missing from it.
    #[test]
    fn a_ledger_breach_discloses_the_stylesheet_rather_than_failing_the_import() {
        let fixture = Fixture::new("breach", &[("index.css", ".a { color: red }\n")]);
        let assets = vec![css_asset(&fixture.path("index.css"))];

        let processed = process_assets(
            &assets,
            test_context_with(&assets, AssetBudgetLimits::exhausted()),
        )
        .expect("a ledger breach must not fail the asset stage");

        assert!(
            processed.contributions.is_empty(),
            "nothing was processed, so nothing may be counted: {processed:?}"
        );
        assert!(
            processed.has_uncounted_assets(),
            "unreached bytes make the result a floor: {processed:?}"
        );
        assert!(
            processed
                .uncounted
                .iter()
                .any(|asset| asset.path == assets[0].path),
            "the stylesheet is disclosed at its raw size: {processed:?}"
        );
    }

    /// The same floor when the breach is detected MID-RUN rather than when the ledger is built: the
    /// union reads its way past the build-wide CSS work limit. Every collected asset is disclosed
    /// exactly once, a direct font included, and nothing is counted.
    #[test]
    fn a_ledger_breach_during_processing_discloses_every_asset_once() {
        let fixture = Fixture::new(
            "midrun-breach",
            &[
                ("a-child.css", ".a-child { color: red }\n"),
                ("b-child.css", ".b-child { color: blue }\n"),
                ("a.css", "@import \"./a-child.css\";\n.a { color: red }\n"),
                ("b.css", "@import \"./b-child.css\";\n.b { color: blue }\n"),
            ],
        );
        let font_path = fixture.write_bytes("probe.woff2", &[0x51; 64]);
        let assets = vec![
            css_asset(&fixture.path("a.css")),
            css_asset(&fixture.path("b.css")),
            read_collected_asset(&font_path, AssetKind::Font).expect("font snapshot"),
        ];
        let context = test_context_with(&assets, AssetBudgetLimits::css_work_reads(3));
        assert!(
            context.failure().is_none(),
            "the premise is a ledger that breaches during processing, not at construction"
        );

        let processed = process_assets(&assets, context)
            .expect("a ledger breach must not fail the asset stage");

        assert!(
            processed.contributions.is_empty(),
            "a breached stage counts nothing: {processed:?}"
        );
        let mut disclosed = processed
            .uncounted
            .iter()
            .map(|asset| asset.path.clone())
            .collect::<Vec<_>>();
        disclosed.sort();
        let mut expected = assets
            .iter()
            .map(|asset| asset.path.clone())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(
            disclosed, expected,
            "each asset exactly once: {processed:?}"
        );
        assert!(
            processed
                .failures
                .iter()
                .any(|failure| failure.contains("CSS read")),
            "the breach is named: {processed:?}"
        );
    }

    /// When the union AND every per-sheet retry fail, each stylesheet is disclosed exactly once, so
    /// the disclosure cannot double its own count and byte total.
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
    /// `uncounted` is empty. The over-count (a shared `@import` inlined into each sheet) is the
    /// accepted cost of degrading, but it must be disclosed.
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
        let context = test_context(&[]);
        let provider = TrackingProvider::new(&[], None, context.clone());

        assert!(provider.read(&child).is_err(), "the first read is missing");
        fixture.write("created.css", ".created { color: red }");
        assert!(provider.read(&child).is_ok(), "the retry can read it");

        let freshness = context.freshness_fingerprints();

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
        );

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
        let processed = process_assets_for_test(&[
            css_asset(&fixture.path("a.css")),
            css_asset(&fixture.path("b.css")),
        ]);
        for name in ["shared.css", "a.css", "b.css"] {
            assert!(
                processed.read_paths.iter().any(|path| path.ends_with(name)),
                "{name} must be captured for freshness: {:?}",
                processed.read_paths,
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

    /// The raw-byte fallback. A dangling `@import` cannot be resolved from disk, so bundling must
    /// error rather than panic and the caller reverts to raw-byte disclosure.
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
            "it must be disclosed with its raw bytes: {processed:?}",
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
