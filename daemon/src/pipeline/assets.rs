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
use crate::ipc::protocol::{ImportDiagnostic, MeasuredSizes};
use crate::pipeline::compress::compress_all_bytes;
use lightningcss::bundler::{Bundler, FileProvider, ResolveResult, SourceProvider};
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions};
use lightningcss::targets::Targets;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A `SourceProvider` that reads from disk like the built-in `FileProvider` but records every path
/// it opens, and can serve one synthetic in-memory entry. The bundler drives the `@import` graph
/// with `rayon`, so the provider must be `Send + Sync`; the capture set is a `Mutex`, never a
/// `RefCell`.
struct TrackingProvider {
    inner: FileProvider,
    /// A virtual entry that `@import`s each reachable stylesheet by absolute path, so N stylesheets
    /// bundle into ONE artifact. `None` when there is a single real entry to bundle directly.
    synthetic: Option<(PathBuf, String)>,
    read_paths: Mutex<HashSet<PathBuf>>,
}

impl TrackingProvider {
    fn new() -> Self {
        Self {
            inner: FileProvider::new(),
            synthetic: None,
            read_paths: Mutex::new(HashSet::new()),
        }
    }

    fn with_synthetic(path: PathBuf, content: String) -> Self {
        Self {
            inner: FileProvider::new(),
            synthetic: Some((path, content)),
            read_paths: Mutex::new(HashSet::new()),
        }
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
        self.read_paths
            .lock()
            .expect("css read-path set should not be poisoned")
            .insert(key);
        self.inner.read(file)
    }

    fn resolve(
        &self,
        specifier: &str,
        originating_file: &Path,
    ) -> Result<ResolveResult, Self::Error> {
        // The synthetic entry `@import`s absolute paths; resolve those directly. `FileProvider`'s
        // own resolve is a naive relative join and would mangle them.
        let candidate = Path::new(specifier);
        if candidate.is_absolute() {
            return Ok(ResolveResult::File(candidate.to_path_buf()));
        }
        self.inner.resolve(specifier, originating_file)
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
            // Forward slashes: a backslash is an escape character inside a CSS string, so a Windows
            // path spelled with them would be mangled before `resolve` ever saw it.
            let specifier = entry.to_string_lossy().replace('\\', "/");
            format!("@import \"{specifier}\";")
        })
        .collect::<Vec<_>>()
        .join("\n");

    (path, content)
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

/// One asset kind's counted contribution: every artifact of that kind, each compressed on its own
/// and summed (ADR-0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetContribution {
    pub kind: AssetKind,
    pub sizes: MeasuredSizes,
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
            total.raw_bytes += contribution.sizes.raw_bytes;
            total.minified_bytes += contribution.sizes.minified_bytes;
            total.gzip_bytes += contribution.sizes.gzip_bytes;
            total.brotli_bytes += contribution.sizes.brotli_bytes;
            total.zstd_bytes += contribution.sizes.zstd_bytes;
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

    let bundled = bundle_css_set(&entries).and_then(|bundle| {
        compress_all_bytes(&bundle.minified_bytes)
            .map_err(|error| format!("failed to compress the bundled stylesheet: {error}"))
            .map(|compressed| (bundle, compressed))
    });

    match bundled {
        Ok((bundle, compressed)) => {
            processed.contributions.push(AssetContribution {
                kind: AssetKind::Css,
                sizes: MeasuredSizes {
                    raw_bytes: bundle.raw_bytes.len() as u64,
                    minified_bytes: bundle.minified_bytes.len() as u64,
                    gzip_bytes: compressed.gzip_bytes,
                    brotli_bytes: compressed.brotli_bytes,
                    zstd_bytes: compressed.zstd_bytes,
                },
            });
            processed.read_paths.extend(bundle.read_paths);
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
        processed
            .contributions
            .push(AssetContribution { kind, sizes });
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
            contribution.sizes.brotli_bytes > 0 && contribution.sizes.minified_bytes > 0,
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
        assert_eq!(processed.total(), contribution.sizes);
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
        assert_eq!(wasm_contribution.sizes.raw_bytes, 4096);
        assert_eq!(wasm_contribution.sizes.minified_bytes, 4096);
        assert!(wasm_contribution.sizes.brotli_bytes > 0);
        assert_eq!(processed.total().raw_bytes, 4096 + 2048);
        assert!(processed.uncounted.is_empty(), "{processed:?}");
    }
}
