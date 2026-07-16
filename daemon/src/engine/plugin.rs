//! Native Rolldown plugin (spec §7.2/§7.3): serves the virtual entry, maps
//! pre-resolved targets, records loaded real paths, and enforces the product
//! resource limits. It must never override linking or tree-shaking semantics
//! (spec §7.4), so no hook ever returns `HookSideEffects` for a real module.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use rolldown::ModuleType;
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookNoopReturn, HookResolveIdArgs,
    HookResolveIdOutput, HookResolveIdReturn, HookUsage, Plugin, PluginContext,
    SharedLoadPluginContext,
};
use rolldown_common::{ModuleInfo, NormalModule};

use super::entry::{TARGET_PREFIX, VIRTUAL_ENTRY_ID};
use super::limits::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES};
use crate::cache::key::{
    FileFingerprint, content_hash, file_fingerprint_from_read_time, read_time_len_mtime_of,
};

/// Per-build state shared with the adapter, which reads it after the bundler
/// finishes. Limit state is monotonic and thread-safe (spec §7.3).
#[derive(Debug, Default)]
pub(super) struct BuildState {
    /// Canonical paths of every module the graph loaded.
    loaded_paths: Mutex<HashSet<PathBuf>>,
    /// Fingerprints captured at the moment each module's bytes were read, keyed
    /// by the same canonical path (§8.3). See `ImportLensPlugin::load`.
    read_time: Mutex<HashMap<PathBuf, FileFingerprint>>,
    /// `fs::canonicalize` is a file-handle open on Windows and both hooks need
    /// the canonical form of the same paths; memoize so each path is resolved
    /// once per build rather than once per consumer.
    canonical: Mutex<HashMap<PathBuf, PathBuf>>,
    total_source_bytes: AtomicUsize,
    limit_breach: Mutex<Option<String>>,
    /// Non-JavaScript modules the graph imported, and their byte counts. See
    /// [`ImportLensPlugin::load`] and [`super::diagnostic_stage::UNCOUNTED_ASSETS`].
    uncounted_assets: Mutex<HashMap<PathBuf, u64>>,
}

impl BuildState {
    /// Canonical form promised by the contract (§5.1), sorted and deduplicated.
    /// Paths are canonicalized as they are recorded, so this only orders them.
    pub(super) fn sorted_loaded_paths(&self) -> Vec<PathBuf> {
        let paths = self
            .loaded_paths
            .lock()
            .expect("loaded-path set should not be poisoned");
        let mut sorted: Vec<PathBuf> = paths.iter().cloned().collect();
        sorted.sort();
        sorted.dedup();
        sorted
    }

    /// Read-time fingerprints, plus the loaded paths that have none — modules the
    /// `load` hook handed back to Rolldown (non-UTF8 binary modules), which the
    /// caller must fingerprint by reading them itself.
    pub(super) fn read_time_fingerprints(&self) -> (Vec<FileFingerprint>, Vec<PathBuf>) {
        let read_time = self
            .read_time
            .lock()
            .expect("read-time fingerprint map should not be poisoned");

        let mut fingerprints: Vec<FileFingerprint> = read_time.values().cloned().collect();
        fingerprints.sort_by(|left, right| left.path.cmp(&right.path));

        let unhashed = self
            .sorted_loaded_paths()
            .into_iter()
            .filter(|path| !read_time.contains_key(path))
            .collect();

        (fingerprints, unhashed)
    }

    /// The non-JavaScript modules this build's graph imported, with their byte counts, sorted for
    /// a stable diagnostic. Their bytes are NOT in the measured size — the size is the JS chunk —
    /// and they DO ship with the package, so the adapter discloses them.
    pub(super) fn sorted_uncounted_assets(&self) -> Vec<(PathBuf, u64)> {
        let assets = self
            .uncounted_assets
            .lock()
            .expect("uncounted-asset map should not be poisoned");
        let mut sorted: Vec<(PathBuf, u64)> = assets
            .iter()
            .map(|(path, bytes)| (path.clone(), *bytes))
            .collect();
        sorted.sort();
        sorted
    }

    /// Canonicalize once per build. A path that no longer resolves (deleted
    /// mid-build) falls back to the resolver's form, matching the previous
    /// behavior.
    fn canonical_path(&self, path: &Path) -> PathBuf {
        if let Some(canonical) = self
            .canonical
            .lock()
            .expect("canonical-path memo should not be poisoned")
            .get(path)
        {
            return canonical.clone();
        }

        // Never hold the lock across the syscall: `canonicalize` opens a file handle on
        // Windows, and these hooks run concurrently across modules, so holding it would
        // serialize every module's canonicalization behind one mutex.
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.canonical
            .lock()
            .expect("canonical-path memo should not be poisoned")
            .insert(path.to_path_buf(), canonical.clone());
        canonical
    }

    pub(super) fn take_breach(&self) -> Option<String> {
        self.limit_breach
            .lock()
            .expect("limit-breach slot should not be poisoned")
            .take()
    }

    /// Keeps the SMALLEST breach message, not the first one to arrive.
    ///
    /// These hooks run on concurrently-spawned module tasks, so "first" means "whichever module the
    /// runtime happened to finish first". A graph that breaches in more than one place — two
    /// oversized modules, or an oversized module and the total-source cap — would then name a
    /// different module on different runs of the same bytes, and that message is durable: a
    /// `module_graph_limit` failure is deterministic, so it is cached (ADR-0006, invariant 3) and
    /// the user is shown its message. The stage was never in doubt here; the message was. Ordering
    /// by content rather than by arrival makes the whole answer a function of the bytes, which is
    /// the same rule `engine::stage::rank` applies to the diagnostics beside it.
    fn record_breach(&self, message: &str) {
        let mut breach = self
            .limit_breach
            .lock()
            .expect("limit-breach slot should not be poisoned");
        match breach.as_deref() {
            Some(recorded) if recorded <= message => {}
            _ => *breach = Some(message.to_owned()),
        }
    }
}

/// One pre-resolved entry the virtual module maps `import-lens:target/<i>` to, carrying its
/// package's **root** manifest.
///
/// Pre-resolving is the point (§6.1): the engine must never re-resolve the bare package
/// specifier. But Rolldown builds a plugin-resolved `ResolvedId`'s `package_json` from
/// `HookResolveIdOutput::package_json_path` and from **nothing else**, so pre-resolving without
/// supplying the manifest leaves the entry module — and only the entry module — with no package
/// metadata at all. Every *transitive* module is resolved by Rolldown and gets the real thing.
///
/// Supplying it is metadata supply, not a semantic override (ADR-0002): we hand Rolldown a
/// manifest, and it alone decides what that manifest means. `side_effects` stays `None` — §7.4
/// reserves the side-effect decision for Rolldown.
///
/// **It is the package-ROOT manifest, and that is not the same manifest for both of Rolldown's
/// lookups.** `sideEffects` is read from the topmost manifest before the `node_modules` boundary —
/// the package root, so our supply is exactly right, and that is why this exists. `"type"` is read
/// from the *nearest* manifest above the file. One field cannot answer both: a package that nests
/// a manifest between its root and its entry (the dual-package `esm/package.json`
/// `{"type":"module"}` layout) still has its entry's module format decided by the root manifest.
/// That gap predates supplying anything and cannot be closed at this API — known issue C6. Do not
/// "fix" it by supplying the nearest manifest instead: that trades a rare format error for a
/// common `sideEffects` error.
///
/// **The two paths must be spelled so Rolldown can relativize one against the other, and that is
/// the whole of this type's job.** Rolldown answers `sideEffects` by
/// `resolved_id.id.relative_path(package_json.realpath().parent())` and matching the *result*
/// against the declared globs (`ecma_module_view_factory.rs`, `lazy_check_side_effects`). It does
/// **not** re-derive the manifest's location: `try_get_package_json_or_create` takes the string we
/// hand it verbatim ("User has the responsibility to ensure `path` is real path if needed"). So the
/// package-relative path Rolldown matches is computed from **our two strings**, and if they do not
/// share a root the relativization silently yields the whole absolute path instead.
///
/// It did not share a root. The id is `entry_path`, which is `fs::canonicalize` output — a Windows
/// **`\\?\` verbatim** path — while the manifest was `package_root.join("package.json")`, built
/// from the non-canonical document path. `sugar_path`'s `relative` splits a Windows root off each
/// side, sees `//?/C:` against `C:`, takes its "different roots" branch and returns the target
/// unchanged. `check_side_effects_for` then matched the globs against
/// `\\?\C:\…\node_modules\refractor\lib\common.js`.
///
/// **That is not a near miss; it is a silent, one-directional corruption of retention, and it hid
/// behind the matcher's own normalisation.** A pattern with no separator, or a `./` prefix, is
/// prefixed with `**/` before matching — and `**/` happily swallows a whole absolute path, so
/// `["index.js"]` still "matched" and every test we had passed. A pattern that **contains a `/`**
/// is used VERBATIM and anchored, so it can **never** match an absolute path. Real packages use
/// that form: `refractor` declares `["lib/all.js","lib/common.js"]` and its entry is
/// `lib/common.js`, so Rolldown tree-shook away ~35 gated `refractor.register(lang)` statements and
/// we reported **30,229 B** for a package that is really **113,152 B** — a 3.7x undercount, from a
/// path *we* handed it.
///
/// The fix is the input, never the badge: [ADR-0002] makes Rolldown the authority on retention
/// *given correct inputs*, and a badge taught to agree with a retention our own plugin corrupted
/// would bless the wrong number. So the manifest path is **canonicalized**, which puts it in the
/// same verbatim spelling as `entry_path` and — just as importantly — resolves the same symlinks:
/// under pnpm, `node_modules/<name>` is a link into the store and a **workspace-linked** package's
/// `node_modules/<name>` is a junction onto `packages/<name>`, so even two non-verbatim paths would
/// not have shared a prefix. Canonical-vs-canonical is the only spelling that relativizes for all
/// three layouts.
///
/// The id is deliberately left **exactly as it is**: `entry_path` is canonicalized upstream because
/// read-time fingerprinting keys on that stable spelling (§8.3), and the `load` hook, the loaded
/// path set and the module contributions all speak it. Nothing here needs the id to change — only
/// the manifest had to come and meet it.
#[derive(Debug)]
struct PreResolvedTarget {
    entry_path: PathBuf,
    /// The **canonical** `<package_root>/package.json`, or `None` when there is none to point at.
    ///
    /// The guard is not caution, it is correctness: Rolldown *reads* this path
    /// (`Resolver::try_get_package_json_or_create`) and an unreadable one fails the whole build
    /// with `UNHANDLEABLE_ERROR: Failed to read or parse package.json`. A `BundleEntry` does not
    /// promise its `package_root` holds a manifest — the pipeline's always does, because that is
    /// how the root was found, but the engine's own qualification fixtures point at bare
    /// directories. Absent a manifest there is simply nothing Rolldown would have found either.
    manifest_path: Option<String>,
}

impl PreResolvedTarget {
    fn for_entry(entry: &super::BundleEntry) -> Self {
        Self {
            // Canonical on both sides or the relativization is junk, and `BundleEntry` promises
            // only an absolute entry, not a canonical one — the pipeline's legacy-fallback
            // resolution joins the manifest field onto the package root without canonicalizing. It
            // is idempotent for the paths that already are canonical, which is nearly all of them,
            // and it does not change what the daemon *tracks*: `load` and `module_parsed`
            // canonicalize every path they record regardless.
            entry_path: std::fs::canonicalize(&entry.entry_path)
                .unwrap_or_else(|_| entry.entry_path.clone()),
            manifest_path: canonical_manifest_path(&entry.package_root)
                .map(|manifest| manifest.to_string_lossy().into_owned()),
        }
    }
}

/// The package manifest, spelled the way the entry id is spelled: canonical.
///
/// `canonicalize` both proves it exists and resolves the links — see [`PreResolvedTarget`] for why
/// both halves are load-bearing. The `is_file` check survives it because a *directory* named
/// `package.json` canonicalizes just as happily as a file, and handing Rolldown a directory to read
/// fails the entire build.
fn canonical_manifest_path(package_root: &Path) -> Option<PathBuf> {
    let manifest = std::fs::canonicalize(package_root.join("package.json")).ok()?;
    manifest.is_file().then_some(manifest)
}

#[derive(Debug)]
pub(super) struct ImportLensPlugin {
    entry_source: String,
    targets: Vec<PreResolvedTarget>,
    state: Arc<BuildState>,
}

impl ImportLensPlugin {
    /// `targets` is indexed BY POSITION: the virtual entry emits `import-lens:target/<i>` for
    /// `entries[i]` and `resolve_id` maps it back with `targets.get(i)`. A file-size build submits
    /// several entries at once, each from a DIFFERENT package, so any reordering here hands one
    /// package's manifest to another package's entry — which does not withhold a declaration, it
    /// applies the wrong one. Never sort, dedup or filter this vector. Row 51 of the construct
    /// matrix is what notices.
    pub(super) fn for_request(request: &super::BundleRequest) -> Self {
        Self {
            entry_source: super::entry::virtual_entry_source(&request.entries),
            targets: request
                .entries
                .iter()
                .map(PreResolvedTarget::for_entry)
                .collect(),
            state: Arc::new(BuildState::default()),
        }
    }

    /// Export enumeration uses the real entry directly (§8.4): no virtual
    /// module to serve, but limits and path recording still apply.
    pub(super) fn passthrough() -> Self {
        Self {
            entry_source: String::new(),
            targets: Vec::new(),
            state: Arc::new(BuildState::default()),
        }
    }

    pub(super) fn state(&self) -> Arc<BuildState> {
        Arc::clone(&self.state)
    }

    fn breach(&self, message: String) -> std::io::Error {
        self.state.record_breach(&message);
        std::io::Error::other(message)
    }
}

impl Plugin for ImportLensPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("import-lens")
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if args.specifier == VIRTUAL_ENTRY_ID {
            return Ok(Some(HookResolveIdOutput::from_id(VIRTUAL_ENTRY_ID)));
        }
        if let Some(index) = args.specifier.strip_prefix(TARGET_PREFIX) {
            let target = index
                .parse::<usize>()
                .ok()
                .and_then(|index| self.targets.get(index));
            let Some(target) = target else {
                return Err(std::io::Error::other(format!(
                    "unknown import-lens target specifier: {}",
                    args.specifier
                ))
                .into());
            };
            // Pre-resolved absolute path (§6.1): never re-resolve the bare
            // package specifier — but hand Rolldown the package manifest it would have
            // found on the way, or the entry module classifies its own side effects from
            // source alone while every module behind it uses the real declaration
            // (see [`PreResolvedTarget`]).
            return Ok(Some(HookResolveIdOutput {
                package_json_path: target.manifest_path.clone(),
                ..HookResolveIdOutput::from_id(target.entry_path.to_string_lossy().into_owned())
            }));
        }
        Ok(None)
    }

    /// Reads real modules itself so their bytes can be fingerprinted at the moment
    /// they are consumed (§8.3).
    ///
    /// The cache stores a size alongside fingerprints of the files it was computed
    /// from. Fingerprinting them *after* the build — by re-reading from disk — means
    /// a file edited during the analysis window is recorded with its NEW bytes
    /// against a size measured from the OLD ones. The entry then never self-heals:
    /// every later freshness probe re-reads the file, matches the stored hash, and
    /// answers `Fresh`, serving the stale size until that file changes again.
    /// Hashing here closes the window — the hash describes exactly the bytes that
    /// were measured — and removes a whole second pass over the graph's bytes.
    ///
    /// The bytes are read raw and hashed before Rolldown transforms anything, so a
    /// `.ts` module hashes to its on-disk content rather than its transformed output,
    /// which is what a later probe will compare against.
    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if args.id == VIRTUAL_ENTRY_ID {
            return Ok(Some(HookLoadOutput {
                code: self.entry_source.as_str().into(),
                module_type: Some(ModuleType::Js),
                ..HookLoadOutput::default()
            }));
        }

        // Rolldown runtime helpers and other synthetic ids are not files. Real module
        // ids are absolute paths; anything else is left to Rolldown.
        let path = Path::new(args.id);
        if !path.is_absolute() {
            return Ok(None);
        }

        // §7.3: reject an oversized module BEFORE reading it. The limit exists to
        // bound memory, so reading first would blow the very bound being enforced.
        // `module_parsed` still enforces it on the transformed source, which also
        // covers modules this hook hands back to Rolldown below.
        let metadata = match tokio::fs::metadata(path).await {
            Ok(metadata) => metadata,
            // Not a readable file (or vanished): let Rolldown produce its own error.
            Err(_) => return Ok(None),
        };
        if metadata.len() as usize > MAX_MODULE_SOURCE_BYTES {
            return Err(self
                .breach(format!(
                    "module {} exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit",
                    path.display()
                ))
                .into());
        }

        // Capture len+mtime from the stat taken BEFORE the read. Stat-after-read would
        // pair the post-edit metadata with a hash of the pre-edit bytes, and the
        // freshness fast path matches on len+mtime alone — so a file rewritten during
        // the read would probe Fresh forever against bytes it was never measured from,
        // which is the very failure this hook exists to prevent.
        let (len, modified_millis) = read_time_len_mtime_of(&metadata);

        let Ok(bytes) = tokio::fs::read(path).await else {
            return Ok(None);
        };
        // Binary modules (wasm, assets) are not UTF-8. Rolldown handles those itself;
        // the caller back-fills their fingerprints from `read_time_fingerprints`.
        let Ok(source) = String::from_utf8(bytes.clone()) else {
            return Ok(None);
        };

        let canonical = self.state.canonical_path(path);
        self.state
            .read_time
            .lock()
            .expect("read-time fingerprint map should not be poisoned")
            .entry(canonical.clone())
            .or_insert_with(|| {
                file_fingerprint_from_read_time(
                    &canonical,
                    len,
                    modified_millis,
                    content_hash(&bytes),
                )
            });

        // A STYLESHEET the package's own entry imports. Rolldown 1.1.5 does not bundle CSS at all
        // — it fails the whole build with `UNSUPPORTED_FEATURE: Bundling CSS is no longer
        // supported` at the LINK stage — so every package whose ESM entry does `import
        // './styles.css'` (most UI kits) could not be measured. Nobody saw it: the pipeline caught
        // the failure and fabricated a size, and deleting that fabricator without this would send
        // all of them to "Size unavailable".
        //
        // `ModuleType::Empty` makes the module link as nothing (and shims any binding imported from
        // it, so `import styles from './x.css'` works too). The JS graph then measures exactly, and
        // the stylesheet's bytes — real bytes, which really do ship — are recorded here and
        // DISCLOSED rather than silently folded into the number or thrown away with it.
        if is_stylesheet(path) {
            self.state
                .uncounted_assets
                .lock()
                .expect("uncounted-asset map should not be poisoned")
                .insert(canonical, len);

            return Ok(Some(HookLoadOutput {
                code: String::new().into(),
                module_type: Some(ModuleType::Empty),
                ..HookLoadOutput::default()
            }));
        }

        Ok(Some(HookLoadOutput {
            code: source.into(),
            // Let Rolldown infer the module type from the extension, exactly as it
            // does when it reads the file itself.
            ..HookLoadOutput::default()
        }))
    }

    async fn module_parsed(
        &self,
        _ctx: &PluginContext,
        module_info: Arc<ModuleInfo>,
        _normal_module: &NormalModule,
    ) -> HookNoopReturn {
        if module_info.id.as_str() == VIRTUAL_ENTRY_ID {
            return Ok(());
        }
        // Rolldown runtime helpers and other non-path ids are not product
        // modules; externals never reach this hook.
        let Some(path) = module_info.id.as_path() else {
            return Ok(());
        };

        let source_bytes = module_info.code.as_ref().map_or(0, |code| code.len());
        if source_bytes > MAX_MODULE_SOURCE_BYTES {
            return Err(self
                .breach(format!(
                    "module {} exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit",
                    path.display()
                ))
                .into());
        }

        let total_bytes = self
            .state
            .total_source_bytes
            .fetch_add(source_bytes, Ordering::Relaxed)
            + source_bytes;
        let max_graph_source_bytes = *MAX_GRAPH_SOURCE_BYTES;
        if total_bytes > max_graph_source_bytes {
            return Err(self
                .breach(format!(
                    "module graph exceeds the {max_graph_source_bytes} byte total source limit"
                ))
                .into());
        }

        let canonical = self.state.canonical_path(path);
        let module_count = {
            let mut paths = self
                .state
                .loaded_paths
                .lock()
                .expect("loaded-path set should not be poisoned");
            paths.insert(canonical);
            paths.len()
        };
        if module_count > MAX_GRAPH_MODULES {
            return Err(self
                .breach(format!(
                    "module graph exceeds the {MAX_GRAPH_MODULES} internal module limit"
                ))
                .into());
        }

        Ok(())
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::ModuleParsed
    }
}

/// A stylesheet the JavaScript graph imports.
///
/// Rolldown 1.1.5 removed CSS bundling outright, so any of these reaching it as a graph module
/// fails the ENTIRE build (`UNSUPPORTED_FEATURE`, at the link stage) rather than merely going
/// uncounted. The list is deliberately narrow: only what a published package's JS entry plausibly
/// imports. Anything else non-JS (a `.wasm` or an image) is not UTF-8, so `load` already hands it
/// back to Rolldown untouched.
fn is_stylesheet(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| {
            matches!(
                extension.as_str(),
                "css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss"
            )
        })
}
