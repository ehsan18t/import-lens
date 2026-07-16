//! Non-JS asset processing (B2): a package's real cost is not only its JavaScript. A UI kit ships
//! CSS; some packages ship wasm or fonts. The engine measures the JS chunk exactly and hands the
//! reachable assets here to be processed the way they actually ship, so their bytes can be folded
//! into the Import Cost rather than merely disclosed.
//!
//! - **CSS** goes through Lightning CSS: resolve the `@import` tree from disk into one stylesheet,
//!   minify, print. That mirrors how CSS ships (one file per entry) and lets Lightning CSS dedupe
//!   shared `@import`s.
//! - **wasm / fonts** have no processor; their shipped size is the raw file bytes (woff2 is already
//!   brotli-internally, so it barely shrinks, which is correct).
//!
//! Every path Lightning CSS opens — the entry and each resolved `@import` child — is captured for
//! cache freshness, so an edit to any of them invalidates the measured size.

use lightningcss::bundler::{Bundler, FileProvider, ResolveResult, SourceProvider};
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, PrinterOptions};
use lightningcss::targets::Targets;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A `SourceProvider` that reads from disk like the built-in `FileProvider` but records every path
/// it opens. The bundler drives the `@import` graph with `rayon`, so the provider must be
/// `Send + Sync`; the capture set is a `Mutex`, never a `RefCell`.
struct TrackingProvider {
    inner: FileProvider,
    read_paths: Mutex<HashSet<PathBuf>>,
}

impl TrackingProvider {
    fn new() -> Self {
        Self {
            inner: FileProvider::new(),
            read_paths: Mutex::new(HashSet::new()),
        }
    }

    /// The set of files Lightning CSS actually read — the entry plus every resolved `@import`
    /// child. Consumed after bundling is done (the provider must outlive the `StyleSheet`).
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
        self.inner.resolve(specifier, originating_file)
    }
}

/// A reachable stylesheet processed as it ships: the minified, `@import`-inlined bytes plus every
/// file that fed them (for freshness).
#[derive(Debug)]
pub struct CssBundle {
    pub bytes: Vec<u8>,
    pub read_paths: Vec<PathBuf>,
}

/// Bundle a CSS entry the way it ships: resolve its `@import` tree from disk into one stylesheet,
/// minify with deterministic (target-free) output, and print. Returns the shipped bytes and the set
/// of files read. Any failure is an `Err`; the caller falls back to raw-byte disclosure so the
/// result never drops below today's behavior.
pub fn bundle_css(entry: &Path) -> Result<CssBundle, String> {
    let provider = TrackingProvider::new();

    // The `StyleSheet` borrows source strings held inside `provider`, so all consumption happens in
    // this scope, before `provider` is moved into `into_read_paths`.
    let bytes = {
        let mut bundler = Bundler::new(&provider, None, ParserOptions::default());
        let mut stylesheet = bundler.bundle(entry).map_err(|error| {
            format!(
                "lightningcss failed to bundle {}: {error:?}",
                entry.display()
            )
        })?;
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
        let printed = stylesheet
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
            })?;
        printed.code.into_bytes()
    };

    Ok(CssBundle {
        bytes,
        read_paths: provider.into_read_paths(),
    })
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

        let css = String::from_utf8(bundle.bytes.clone()).expect("utf8");
        // The `@import` child is inlined, both rules survive, and the whitespace is minified away.
        assert!(
            css.contains(".child"),
            "the @import child must be inlined: {css}"
        );
        assert!(css.contains(".entry"), "the entry rule must survive: {css}");
        assert!(!css.contains("  "), "output must be minified: {css}");
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
}
