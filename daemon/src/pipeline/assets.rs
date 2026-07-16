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
//! Every path Lightning CSS opens — the entry and each resolved `@import` child — is captured for
//! cache freshness, so an edit to any of them invalidates the measured size. Any processing failure
//! falls back to disclosing the raw bytes, which is exactly today's behaviour: never below it
//! ([ADR-0006](../../../docs/adr/0006-the-result-model.md)).

use crate::engine::{AssetKind, CollectedAsset, UncountedAsset, diagnostic_stage};
use crate::ipc::protocol::{AssetContribution, ImportDiagnostic, MeasuredSizes};
use crate::pipeline::compress::{CompressionSizes, compress_all_bytes};
use lightningcss::bundler::{Bundler, FileProvider, ResolveResult, SourceProvider};
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions};
use lightningcss::targets::Targets;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// A `SourceProvider` that reads from disk like the built-in `FileProvider` but records every path
/// it opens, bounds what one `@import` tree may pull in, and can serve one synthetic in-memory
/// entry. The bundler drives the `@import` graph with `rayon`, so the provider must be
/// `Send + Sync`; its state is behind `Mutex`, never a `RefCell`.
struct TrackingProvider {
    inner: FileProvider,
    /// A virtual entry that `@import`s each reachable stylesheet by absolute path, so N stylesheets
    /// bundle into ONE artifact. `None` when there is a single real entry to bundle directly.
    synthetic: Option<(PathBuf, String)>,
    read_paths: Mutex<HashSet<PathBuf>>,
    budget: Mutex<ReadBudget>,
}

impl TrackingProvider {
    fn new() -> Self {
        Self {
            inner: FileProvider::new(),
            synthetic: None,
            read_paths: Mutex::new(HashSet::new()),
            budget: Mutex::new(ReadBudget::default()),
        }
    }

    fn with_synthetic(path: PathBuf, content: String) -> Self {
        Self {
            inner: FileProvider::new(),
            synthetic: Some((path, content)),
            read_paths: Mutex::new(HashSet::new()),
            budget: Mutex::new(ReadBudget::default()),
        }
    }

    /// Charge one file against the tree's budget, refusing once it is spent.
    fn charge(&self, bytes: usize) -> Result<(), std::io::Error> {
        let mut budget = self
            .budget
            .lock()
            .expect("css read budget should not be poisoned");
        budget.files += 1;
        budget.bytes += bytes;

        if budget.files > MAX_STYLESHEET_FILES || budget.bytes > MAX_STYLESHEET_BYTES {
            return Err(std::io::Error::other(format!(
                "stylesheet @import tree exceeds the {MAX_STYLESHEET_FILES} file / \
                 {MAX_STYLESHEET_BYTES} byte limit"
            )));
        }
        Ok(())
    }

    /// The set of real files Lightning CSS read — the entries plus every resolved `@import` child.
    /// Consumed after bundling is done (the provider must outlive the `StyleSheet`).
    fn into_read_paths(self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .read_paths
            .into_inner()
            .expect("css read-path set should not be poisoned")
            .into_iter()
            .collect();
        paths.sort();
        paths
    }
}

impl SourceProvider for TrackingProvider {
    type Error = std::io::Error;

    fn read<'a>(&'a self, file: &Path) -> Result<&'a str, Self::Error> {
        // The synthetic entry has no file behind it, so it is served from memory and never recorded
        // as a freshness input — there is nothing on disk that could change.
        if let Some((path, content)) = &self.synthetic
            && file == path
        {
            return Ok(content.as_str());
        }

        // Canonicalize so a cache key is stable across `..` / symlink spellings of the same file.
        let key = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        let source = self.inner.read(file)?;
        self.charge(source.len())?;
        self.read_paths
            .lock()
            .expect("css read-path set should not be poisoned")
            .insert(key);
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
}

/// Bundle one stylesheet the way it ships: resolve its `@import` tree from disk into one
/// stylesheet, minify with deterministic (target-free) output, and print. Returns the bytes and the
/// set of files read. Any failure is an `Err`; the caller falls back to raw-byte disclosure so the
/// result never drops below today's behavior.
pub fn bundle_css(entry: &Path) -> Result<CssBundle, String> {
    let provider = TrackingProvider::new();
    let (raw_bytes, minified_bytes) = bundle_with(&provider, entry)?;
    Ok(CssBundle {
        raw_bytes,
        minified_bytes,
        read_paths: provider.into_read_paths(),
    })
}

/// Bundle EVERY reachable stylesheet into one artifact, which is how CSS ships and how the esbuild
/// oracle emits it. A single entry bundles directly; several are combined behind a synthetic entry
/// that `@import`s each, so Lightning CSS inlines and dedupes them into one sheet rather than us
/// summing overlapping copies.
pub fn bundle_css_set(entries: &[PathBuf]) -> Result<CssBundle, String> {
    match entries {
        [] => Err("no stylesheets to bundle".to_owned()),
        [single] => bundle_css(single),
        many => {
            let (path, content) = synthetic_entry(many);
            let provider = TrackingProvider::with_synthetic(path.clone(), content);
            let (raw_bytes, minified_bytes) = bundle_with(&provider, &path)?;
            Ok(CssBundle {
                raw_bytes,
                minified_bytes,
                read_paths: provider.into_read_paths(),
            })
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
fn bundle_with(provider: &TrackingProvider, entry: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut bundler = Bundler::new(provider, None, ParserOptions::default());
    let mut stylesheet = bundler.bundle(entry).map_err(|error| {
        format!(
            "lightningcss failed to bundle {}: {error:?}",
            entry.display()
        )
    })?;

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

    Ok((raw_bytes, minified_bytes))
}

/// What the reachable assets really cost, ready to fold into the Import Cost.
#[derive(Debug, Default)]
pub struct ProcessedAssets {
    /// One entry per asset kind actually present, already summed across that kind's artifacts.
    pub contributions: Vec<AssetContribution>,
    /// Files the processing read that the build did not already know about — a stylesheet's
    /// `@import` children. Without these in the freshness fingerprints, editing one would not
    /// invalidate the size it fed.
    pub read_paths: Vec<PathBuf>,
    /// Assets that could NOT be processed, disclosed with their raw bytes exactly as before.
    pub uncounted: Vec<UncountedAsset>,
    /// Why each of those fell back, for the diagnostic.
    pub failures: Vec<String>,
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

/// Process every reachable asset the build collected, the way each really ships.
///
/// Never fails: an asset it cannot process falls back to the raw-byte disclosure that was the whole
/// behaviour before B2, so the result is a strict improvement or a tie, never a regression.
pub fn process_assets(assets: &[CollectedAsset]) -> ProcessedAssets {
    let mut processed = ProcessedAssets::default();
    if assets.is_empty() {
        return processed;
    }

    process_stylesheets(assets, &mut processed);
    for kind in [AssetKind::Wasm, AssetKind::Font] {
        process_binary_kind(assets, kind, &mut processed);
    }

    processed
        .contributions
        .sort_by_key(|contribution| contribution.kind);
    processed
}

fn process_stylesheets(assets: &[CollectedAsset], processed: &mut ProcessedAssets) {
    let entries: Vec<PathBuf> = assets
        .iter()
        .filter(|asset| asset.kind == AssetKind::Css)
        .map(|asset| asset.path.clone())
        .collect();
    if entries.is_empty() {
        return;
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
    let bundled = bundle_css_set(&entries)
        .and_then(compress_bundle)
        .map(|counted| (vec![counted], Vec::new(), Vec::new()))
        .or_else(|union_error| {
            if entries.len() == 1 {
                return Err(union_error);
            }

            let mut counted = Vec::new();
            let mut failures = vec![format!(
                "bundling the stylesheets as one artifact failed, so each was measured on its own \
                 and shared @imports are counted once per sheet: {union_error}"
            )];
            let mut uncounted = Vec::new();

            for entry in &entries {
                match bundle_css(entry).and_then(compress_bundle) {
                    Ok(bundled) => counted.push(bundled),
                    Err(error) => {
                        failures.push(error);
                        uncounted.push(UncountedAsset {
                            path: entry.clone(),
                            bytes: assets
                                .iter()
                                .find(|asset| &asset.path == entry)
                                .map_or(0, |asset| asset.raw_bytes),
                        });
                    }
                }
            }

            // Every sheet failed, so this is simply the pre-B2 fallback: hand back the union's error
            // and let the one disclosure below cover them, exactly once each.
            if counted.is_empty() {
                return Err(union_error);
            }
            Ok((counted, failures, uncounted))
        });

    match bundled {
        Ok((counted, failures, uncounted)) => {
            processed.failures.extend(failures);
            processed.uncounted.extend(uncounted);
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
                processed.read_paths.extend(bundle.read_paths);
            }
            processed.contributions.push(css);
        }
        Err(message) => {
            processed.failures.push(message);
            processed.uncounted.extend(
                assets
                    .iter()
                    .filter(|asset| asset.kind == AssetKind::Css)
                    .map(|asset| UncountedAsset {
                        path: asset.path.clone(),
                        bytes: asset.raw_bytes,
                    }),
            );
        }
    }
}

/// Compress a bundled stylesheet as its own artifact — never concatenated with anything else first,
/// because it ships as its own file (ADR-0005).
fn compress_bundle(bundle: CssBundle) -> Result<(CssBundle, CompressionSizes), String> {
    compress_all_bytes(&bundle.minified_bytes)
        .map_err(|error| format!("failed to compress the bundled stylesheet: {error}"))
        .map(|compressed| (bundle, compressed))
}

fn process_binary_kind(
    assets: &[CollectedAsset],
    kind: AssetKind,
    processed: &mut ProcessedAssets,
) {
    let mut sizes = MeasuredSizes::ZERO;
    let mut counted = false;

    for asset in assets.iter().filter(|asset| asset.kind == kind) {
        let measured = std::fs::read(&asset.path)
            .map_err(|error| format!("failed to read {}: {error}", asset.path.display()))
            .and_then(|bytes| {
                compress_all_bytes(&bytes)
                    .map_err(|error| {
                        format!("failed to compress {}: {error}", asset.path.display())
                    })
                    .map(|compressed| (bytes.len() as u64, compressed))
            });

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
                processed.failures.push(message);
                processed.uncounted.push(UncountedAsset {
                    path: asset.path.clone(),
                    bytes: asset.raw_bytes,
                });
            }
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

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("il-assets-{}-{tag}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn css_asset(path: &Path) -> CollectedAsset {
        CollectedAsset {
            path: path.to_path_buf(),
            kind: AssetKind::Css,
            raw_bytes: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
        }
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
    /// applied to them. The bound is on BREADTH, which is what it can honestly measure — depth is
    /// answered by giving the walk its own stack, not by counting files.
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
        let css = String::from_utf8(bundle.minified_bytes).expect("utf8");
        assert!(
            css.contains(".a"),
            "the local rules must still be counted: {css}"
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
        let assets = vec![
            CollectedAsset {
                path: good.clone(),
                kind: AssetKind::Css,
                raw_bytes: fs::metadata(&good).map(|meta| meta.len()).unwrap_or(0),
            },
            CollectedAsset {
                path: bad.clone(),
                kind: AssetKind::Css,
                raw_bytes: fs::metadata(&bad).map(|meta| meta.len()).unwrap_or(0),
            },
        ];

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
            vec![&bad],
            "only the offender falls back to disclosure: {processed:?}",
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
            .map(|path| CollectedAsset {
                path: (*path).clone(),
                kind: AssetKind::Css,
                raw_bytes: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
            })
            .collect();

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
        assert_eq!(disclosed, vec![&first, &second], "{processed:?}");
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
        fs::remove_dir_all(&dir).ok();

        assert!(
            processed.contributions.is_empty(),
            "an unprocessable stylesheet must not be counted: {processed:?}",
        );
        assert_eq!(
            processed.uncounted,
            vec![UncountedAsset {
                path: asset.path,
                bytes: asset.raw_bytes
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
            CollectedAsset {
                path: wasm,
                kind: AssetKind::Wasm,
                raw_bytes: 4096,
            },
            CollectedAsset {
                path: font,
                kind: AssetKind::Font,
                raw_bytes: 2048,
            },
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
