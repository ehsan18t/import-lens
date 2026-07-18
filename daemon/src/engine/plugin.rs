//! Native Rolldown plugin (spec §7.2/§7.3): serves the virtual entry, maps
//! pre-resolved targets, records loaded real paths, and enforces the product
//! resource limits. It must never override linking or tree-shaking semantics
//! (spec §7.4), so no hook ever returns `HookSideEffects` for a real module.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
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
    PluginContextResolveOptions, SharedLoadPluginContext,
};
use rolldown_common::{ModuleInfo, NormalModule};

use super::entry::{TARGET_PREFIX, VIRTUAL_ENTRY_ID};
use super::limits::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES};
use super::{AssetClass, CollectedAsset, UncountedAsset, classify_asset_class};
use crate::cache::key::{
    FileFingerprint, content_hash, file_fingerprint_from_read_time, read_time_len_mtime_of,
    sort_and_dedup_fingerprints, unverifiable_file_fingerprint,
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
    /// Classified non-JavaScript modules the graph imported, keyed by canonical path. See
    /// [`ImportLensPlugin::load`]; the pipeline processes them and counts their shipped bytes (B2).
    assets: Mutex<HashMap<PathBuf, CollectedAsset>>,
    /// Classified assets whose metadata or byte read failed in this plugin. Even if Rolldown later
    /// succeeds through its own loader, that mixed observation describes this filesystem moment,
    /// not a reusable fact about the package.
    failed_asset_inputs: Mutex<HashSet<PathBuf>>,
    /// Directly imported files that ship but are outside the measured taxonomy — an image, an icon.
    /// Stubbed so they cannot fail the build, and disclosed so their bytes are not silently absent.
    unmeasured_assets: Mutex<BTreeMap<PathBuf, UncountedAsset>>,
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

    /// The classified non-JavaScript modules this build's graph imported, sorted for a stable
    /// result. Their bytes are NOT in the JavaScript chunk and they DO ship with the package, so
    /// the pipeline processes them the way they ship and folds the result into the size (B2).
    pub(super) fn sorted_assets(&self) -> Vec<CollectedAsset> {
        let assets = self
            .assets
            .lock()
            .expect("asset map should not be poisoned");
        let mut sorted: Vec<CollectedAsset> = assets.values().cloned().collect();
        sorted.sort_by(|left, right| left.path.cmp(&right.path));
        sorted
    }

    /// Returns whether this call is the one that claimed the path. `false` means a duplicate hook
    /// invocation, whose byte reservation the caller must release — the counted asset map reports
    /// the same thing for the same reason.
    fn record_unmeasured_asset(&self, asset: UncountedAsset) -> bool {
        self.unmeasured_assets
            .lock()
            .expect("unmeasured asset map should not be poisoned")
            .insert(asset.path.clone(), asset)
            .is_none()
    }

    /// Disclosed, deduplicated by path so two imports of the same icon are one disclosure.
    pub(super) fn unmeasured_assets(&self) -> Vec<UncountedAsset> {
        self.unmeasured_assets
            .lock()
            .expect("unmeasured asset map should not be poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub(super) fn record_failed_asset_input(&self, path: PathBuf) {
        self.failed_asset_inputs
            .lock()
            .expect("failed asset-input set should not be poisoned")
            .insert(path);
    }

    pub(super) fn failed_asset_paths(&self) -> Vec<PathBuf> {
        let mut paths = self
            .failed_asset_inputs
            .lock()
            .expect("failed asset-input set should not be poisoned")
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Never-fresh observations that keep a machine-dependent asset failure out of every cache.
    pub(super) fn unverifiable_asset_fingerprints(&self) -> Vec<FileFingerprint> {
        let mut fingerprints = self
            .failed_asset_paths()
            .into_iter()
            .map(unverifiable_file_fingerprint)
            .collect::<Vec<_>>();
        sort_and_dedup_fingerprints(&mut fingerprints);
        fingerprints
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

    /// Source bytes admitted by this build after every direct-asset reservation has been
    /// reconciled with the bytes actually read.
    pub(super) fn graph_source_bytes(&self) -> usize {
        self.total_source_bytes.load(Ordering::Relaxed)
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
    pub(super) fn record_breach(&self, message: &str) {
        let mut breach = self
            .limit_breach
            .lock()
            .expect("limit-breach slot should not be poisoned");
        match breach.as_deref() {
            Some(recorded) if recorded <= message => {}
            _ => *breach = Some(message.to_owned()),
        }
    }

    fn record_fingerprint(&self, path: PathBuf, fingerprint: FileFingerprint) {
        self.read_time
            .lock()
            .expect("read-time fingerprint map should not be poisoned")
            .entry(path)
            .or_insert(fingerprint);
    }

    /// Record the exact stat snapshot that made a pre-read limit failure deterministic. A hash is
    /// unnecessary here: an equal-length/equal-mtime rewrite cannot change whether the same byte
    /// ceiling is breached, while any metadata change expires the cached failure.
    fn record_stat_fingerprint(&self, canonical: &Path, metadata: &std::fs::Metadata) {
        let (len, modified_millis) = read_time_len_mtime_of(metadata);
        self.record_fingerprint(
            canonical.to_path_buf(),
            FileFingerprint {
                path: canonical.to_string_lossy().replace('\\', "/"),
                len,
                modified_millis,
                content_hash: None,
            },
        );
    }

    /// Returns whether this read won the canonical asset slot. Rolldown normally loads one module
    /// identity once, but treating the map as the authority prevents a duplicate hook invocation
    /// from leaving the aggregate source counter double-charged.
    fn record_asset(&self, asset: CollectedAsset) -> bool {
        let path = asset.path.clone();
        // Duplicate imports may reach this hook concurrently. Derive the fingerprint from the
        // snapshot that actually won the asset-map entry so the two maps can never describe
        // different reads of the same path.
        let (fingerprint, inserted) = {
            let mut assets = self
                .assets
                .lock()
                .expect("asset map should not be poisoned");
            match assets.entry(path.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    (entry.get().fingerprint.clone(), false)
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let fingerprint = asset.fingerprint.clone();
                    entry.insert(asset);
                    (fingerprint, true)
                }
            }
        };
        self.record_fingerprint(path.clone(), fingerprint);
        inserted
    }
}

fn supported_asset_observation_candidate(specifier: &str, importer: &str) -> Option<PathBuf> {
    let query = specifier.find('?');
    // A leading `#` is a package-import specifier, not a URL fragment. A later `#` is still a
    // loader-style fragment and is removed before extension classification.
    let fragment = if let Some(package_import) = specifier.strip_prefix('#') {
        package_import.find('#').map(|index| index + 1)
    } else {
        specifier.find('#')
    };
    let path_end = query
        .into_iter()
        .chain(fragment)
        .min()
        .unwrap_or(specifier.len());
    let specifier_path = Path::new(&specifier[..path_end]);
    classify_asset_class(specifier_path)?;
    if specifier_path.is_absolute() {
        return Some(specifier_path.to_path_buf());
    }
    let is_package_relative = specifier.starts_with("./") || specifier.starts_with("../");
    if !is_package_relative {
        // Bare/self-referential/aliased specifiers have no honest filesystem candidate until the
        // configured resolver answers. The spelling is still useful in the disclosure, and the
        // resulting unverifiable sentinel is rejected by identity rather than by probing this path.
        return Some(specifier_path.to_path_buf());
    }
    let importer = Path::new(importer);
    if !importer.is_absolute() {
        return None;
    }
    let parent = importer.parent()?;
    Some(parent.join(specifier_path))
}

/// Atomically reserve bytes without ever moving the counter past `limit` on rejection.
///
/// `fetch_add` is not suitable for a hard resource ceiling: it mutates first, so every rejected
/// module permanently inflates the total and can manufacture follow-on breaches. `fetch_update`
/// makes the check and increment one compare/exchange operation and leaves the counter untouched
/// when the reservation does not fit.
fn try_reserve_source_bytes(total: &AtomicUsize, bytes: usize, limit: usize) -> Result<(), usize> {
    total
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current
                .checked_add(bytes)
                .filter(|candidate| *candidate <= limit)
        })
        .map(|_| ())
}

fn release_source_bytes(total: &AtomicUsize, bytes: usize) {
    total
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_sub(bytes)
        })
        .expect("released source bytes must have an existing reservation");
}

/// Replace a metadata reservation with the exact length returned by the read. A shrinking file
/// releases capacity; a growing file must atomically acquire the difference before its bytes can
/// enter the artifact.
fn reconcile_source_bytes(
    total: &AtomicUsize,
    reserved: usize,
    actual: usize,
    limit: usize,
) -> Result<(), usize> {
    match actual.cmp(&reserved) {
        std::cmp::Ordering::Less => {
            release_source_bytes(total, reserved - actual);
            Ok(())
        }
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => try_reserve_source_bytes(total, actual - reserved, limit),
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

    fn reserve_source_bytes(&self, source_bytes: usize) -> Result<(), std::io::Error> {
        let max_graph_source_bytes = *MAX_GRAPH_SOURCE_BYTES;
        if try_reserve_source_bytes(
            &self.state.total_source_bytes,
            source_bytes,
            max_graph_source_bytes,
        )
        .is_err()
        {
            return Err(self.breach(format!(
                "module graph exceeds the {max_graph_source_bytes} byte total source limit"
            )));
        }
        Ok(())
    }

    fn release_source_bytes(&self, source_bytes: usize) {
        release_source_bytes(&self.state.total_source_bytes, source_bytes);
    }

    fn reconcile_source_bytes(&self, reserved: usize, actual: usize) -> Result<(), std::io::Error> {
        if reconcile_source_bytes(
            &self.state.total_source_bytes,
            reserved,
            actual,
            *MAX_GRAPH_SOURCE_BYTES,
        )
        .is_err()
        {
            return Err(self.breach(format!(
                "module graph exceeds the {} byte total source limit",
                *MAX_GRAPH_SOURCE_BYTES
            )));
        }
        Ok(())
    }

    /// Capture len+mtime from the stat taken BEFORE the read, paired with a hash of the bytes we
    /// actually read, so freshness describes the bytes the size was measured from (§8.3).
    fn record_read_time(&self, canonical: &Path, len: u64, modified_millis: u64, bytes: &[u8]) {
        self.state.record_fingerprint(
            canonical.to_path_buf(),
            file_fingerprint_from_read_time(canonical, len, modified_millis, content_hash(bytes)),
        );
    }
}

impl Plugin for ImportLensPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("import-lens")
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
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
        if let Some(importer) = args.importer
            && let Some(candidate) = supported_asset_observation_candidate(args.specifier, importer)
        {
            // Ask Rolldown's configured resolver (with this hook skipped) rather than joining a
            // relative path ourselves. Client/Component builds apply package `browser` aliases
            // here, and a raw join would silently measure the server asset or ignore a `false`
            // mapping. Taking the successful result back through this hook still guarantees its
            // final id reaches our observing `load`; retaining an asset-looking specifier on
            // failure closes the resolve/load race for relative, absolute, bare, and aliased forms
            // without changing resolver semantics.
            let resolved = ctx
                .resolve(
                    args.specifier,
                    Some(importer),
                    Some(PluginContextResolveOptions {
                        import_kind: args.kind,
                        is_entry: args.is_entry,
                        skip_self: true,
                        custom: Arc::clone(&args.custom),
                    }),
                )
                .await?;
            match resolved {
                Ok(resolved) => {
                    return Ok(Some(HookResolveIdOutput::from_resolved_id(resolved)));
                }
                Err(_) => {
                    self.state.record_failed_asset_input(candidate);
                    // Let the normal resolver run once more so Rolldown retains its native resolve
                    // diagnostic. `classify_failure` promotes our typed `asset_io` observation.
                    return Ok(None);
                }
            }
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
        let asset_class = classify_asset_class(path);
        let asset_kind = match asset_class {
            Some(AssetClass::Counted(kind)) => Some(kind),
            _ => None,
        };

        // §7.3: reject an oversized module BEFORE reading it. The limit exists to
        // bound memory, so reading first would blow the very bound being enforced.
        // `module_parsed` still enforces it on the transformed source, which also
        // covers modules this hook hands back to Rolldown below.
        // Resolve the identity BEFORE the stat/read pair. Canonicalizing after the read can pair
        // bytes from an old symlink target with the path of a newly-retargeted one.
        let canonical = self.state.canonical_path(path);
        let metadata = match tokio::fs::metadata(&canonical).await {
            Ok(metadata) => metadata,
            Err(error) => {
                if asset_class.is_some() {
                    self.state.record_failed_asset_input(canonical);
                    // Do not let the default loader reopen a recovering/growing asset outside this
                    // plugin's source-byte reservations. The adapter promotes the retained cause
                    // to `asset_io`, while this error keeps the build from consuming unobserved
                    // bytes on a second path.
                    return Err(error.into());
                }
                return Ok(None);
            }
        };
        if metadata.len() > MAX_MODULE_SOURCE_BYTES as u64 {
            self.state.record_stat_fingerprint(&canonical, &metadata);
            return Err(self
                .breach(format!(
                    "module {} exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit",
                    canonical.display()
                ))
                .into());
        }

        // Capture len+mtime from the stat taken BEFORE the read. Stat-after-read would
        // pair the post-edit metadata with a hash of the pre-edit bytes, and the
        // freshness fast path matches on len+mtime alone — so a file rewritten during
        // the read would probe Fresh forever against bytes it was never measured from,
        // which is the very failure this hook exists to prevent.
        let (len, modified_millis) = read_time_len_mtime_of(&metadata);

        // Direct assets become empty Rolldown modules, so `module_parsed` sees zero bytes for them.
        // Reserve the stat length here, BEFORE reading, both to make the aggregate cap cover them
        // and to keep a static oversized asset from allocating past the bound it is about to fail.
        // The per-file check above makes this conversion safe on every supported architecture.
        let reserved_asset_bytes = if asset_class.is_some() {
            let metadata_bytes = usize::try_from(metadata.len())
                .expect("a per-file-admitted asset length must fit usize");
            if let Err(error) = self.reserve_source_bytes(metadata_bytes) {
                self.state.record_stat_fingerprint(&canonical, &metadata);
                return Err(error.into());
            }
            Some(metadata_bytes)
        } else {
            None
        };

        let bytes = match tokio::fs::read(&canonical).await {
            Ok(bytes) => bytes,
            Err(error) => {
                if let Some(reserved) = reserved_asset_bytes {
                    self.release_source_bytes(reserved);
                }
                if asset_class.is_some() {
                    self.state.record_failed_asset_input(canonical);
                    return Err(error.into());
                }
                return Ok(None);
            }
        };

        // A non-JavaScript ASSET the package's own entry imports, intercepted BEFORE the UTF-8
        // conversion below — a wasm or font is not UTF-8, and handing one back to Rolldown lets it
        // perturb or fail the JS build, which is the number we need exact.
        //
        // Stylesheets have their own reason: Rolldown 1.1.5 does not bundle CSS at all (it fails
        // the whole build with `UNSUPPORTED_FEATURE` at the LINK stage), so every package whose ESM
        // entry does `import './styles.css'` (most UI kits) could not be measured.
        //
        // `ModuleType::Empty` makes the module link as nothing (and shims any binding imported from
        // it, so `import styles from './x.css'` works too), so the JS graph measures exactly. The
        // asset itself is recorded here with its kind, and the pipeline then processes it the way
        // it really ships and folds those bytes into the Import Cost (B2) — they are neither
        // fabricated into the JS number nor thrown away with it.
        if let Some(kind) = asset_kind {
            let reserved = reserved_asset_bytes
                .expect("a classified asset must reserve its metadata length before reading");
            let asset = CollectedAsset::from_read(canonical, kind, &metadata, bytes);
            let actual = asset.bytes().len();

            // A file may grow between metadata and read. The pre-read check is still the memory
            // guard for stable files; this exact post-read check closes the concurrent-growth gap
            // and fingerprints the bytes that made the deterministic failure true.
            if actual > MAX_MODULE_SOURCE_BYTES {
                self.state
                    .record_fingerprint(asset.path.clone(), asset.fingerprint.clone());
                self.release_source_bytes(reserved);
                return Err(self
                    .breach(format!(
                        "module {} exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit",
                        asset.path.display()
                    ))
                    .into());
            }

            if let Err(error) = self.reconcile_source_bytes(reserved, actual) {
                self.state
                    .record_fingerprint(asset.path.clone(), asset.fingerprint.clone());
                // A failed growth reservation leaves the original metadata reservation intact.
                self.release_source_bytes(reserved);
                return Err(error.into());
            }

            if !self.state.record_asset(asset) {
                self.release_source_bytes(actual);
            }

            return Ok(Some(HookLoadOutput {
                code: String::new().into(),
                module_type: Some(ModuleType::Empty),
                ..HookLoadOutput::default()
            }));
        }

        // A file that ships but is outside the measured taxonomy — an image, an icon, a media file.
        //
        // It is intercepted for the same reason a font is: left to Rolldown, ONE of these makes the
        // whole package unmeasurable. A `.png` is not UTF-8, so its loader fails on `InvalidData`;
        // an `.svg` IS valid UTF-8, so it is handed to OXC and parsed as JavaScript, which fails
        // differently and just as fatally. The user saw "unavailable" for a package whose
        // JavaScript we could measure perfectly.
        //
        // Stubbing it to `Empty` lets the JS graph measure exactly, and the bytes are DISCLOSED
        // rather than dropped: they ship, so a size that omits them is a floor and has to say so.
        // Its length is charged against the graph's aggregate ceiling like any other asset, so
        // stubbing cannot become a way to admit bytes no limit ever sees.
        if asset_class == Some(AssetClass::Unmeasured) {
            let reserved = reserved_asset_bytes
                .expect("a classified asset must reserve its metadata length before reading");
            let actual = bytes.len();

            // Same post-read growth check the counted arm makes. The pre-read stat bounds a stable
            // file; this closes the window where it grew between the stat and the read, and it
            // fingerprints the bytes that made the deterministic failure true.
            if actual > MAX_MODULE_SOURCE_BYTES {
                self.record_read_time(&canonical, len, modified_millis, &bytes);
                self.release_source_bytes(reserved);
                return Err(self
                    .breach(format!(
                        "module {} exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit",
                        canonical.display()
                    ))
                    .into());
            }

            if let Err(error) = self.reconcile_source_bytes(reserved, actual) {
                // Record the fingerprint BEFORE returning, as the counted arm does: this failure is
                // deterministic and cacheable, and without the fingerprint it would not expire when
                // the file that caused it changes.
                self.record_read_time(&canonical, len, modified_millis, &bytes);
                self.release_source_bytes(reserved);
                return Err(error.into());
            }
            self.record_read_time(&canonical, len, modified_millis, &bytes);

            // Release on a DUPLICATE, exactly as the counted arm does. Two module ids can
            // canonicalize to one path (a pnpm symlink layout is the ordinary shape), and both
            // charge their length against the aggregate ceiling. Only the first is ever accounted
            // for, so without this the counter drifts up for the rest of the build.
            if !self.state.record_unmeasured_asset(UncountedAsset {
                path: canonical,
                bytes: actual as u64,
            }) {
                self.release_source_bytes(actual);
            }

            return Ok(Some(HookLoadOutput {
                code: String::new().into(),
                module_type: Some(ModuleType::Empty),
                ..HookLoadOutput::default()
            }));
        }

        // A binary module that is NOT a classified asset. Rolldown handles those itself; the caller
        // back-fills their fingerprints from `read_time_fingerprints`.
        let Ok(source) = String::from_utf8(bytes.clone()) else {
            return Ok(None);
        };

        self.record_read_time(&canonical, len, modified_millis, &bytes);

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

        self.reserve_source_bytes(source_bytes)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_asset_specifiers_become_observation_candidates_without_reinterpreting_dot_names() {
        let importer = std::env::temp_dir().join("pkg").join("index.js");
        let importer_text = importer.to_string_lossy();
        assert_eq!(
            supported_asset_observation_candidate("./font.woff2?url", &importer_text),
            Some(importer.parent().unwrap().join("font.woff2"))
        );
        assert!(supported_asset_observation_candidate("./helper.js", &importer_text).is_none());
        assert_eq!(
            supported_asset_observation_candidate("asset-pkg/font.woff2", &importer_text),
            Some(PathBuf::from("asset-pkg/font.woff2"))
        );
        assert_eq!(
            supported_asset_observation_candidate("#font.woff2", &importer_text),
            Some(PathBuf::from("#font.woff2"))
        );
        assert!(supported_asset_observation_candidate("./font.woff2", "virtual:entry").is_none());
        assert!(
            supported_asset_observation_candidate(".font.woff2", &importer_text).is_some(),
            "a bare hidden-name specifier stays bare; it is observed but never joined to importer"
        );
        assert_eq!(
            supported_asset_observation_candidate(".font.woff2", &importer_text),
            Some(PathBuf::from(".font.woff2"))
        );
        assert_eq!(
            supported_asset_observation_candidate(".../font.woff2", &importer_text),
            Some(PathBuf::from(".../font.woff2"))
        );
    }

    #[test]
    fn failed_asset_observations_are_never_reusable() {
        let state = BuildState::default();
        let failed_read = PathBuf::from("/pkg/read-failed.woff2");
        state.record_failed_asset_input(failed_read.clone());

        let observations = state.unverifiable_asset_fingerprints();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].path,
            failed_read.to_string_lossy().replace('\\', "/")
        );
        assert!(
            crate::cache::key::fingerprint_is_unverifiable(&observations[0]),
            "a read failure must prevent cache admission"
        );
    }

    #[test]
    fn rejected_source_reservation_never_inflates_the_total() {
        let total = AtomicUsize::new(8);

        assert_eq!(try_reserve_source_bytes(&total, 3, 10), Err(8));
        assert_eq!(total.load(Ordering::Relaxed), 8);

        let near_overflow = AtomicUsize::new(usize::MAX - 1);
        assert_eq!(
            try_reserve_source_bytes(&near_overflow, 2, usize::MAX),
            Err(usize::MAX - 1)
        );
        assert_eq!(near_overflow.load(Ordering::Relaxed), usize::MAX - 1);
    }

    #[test]
    fn concurrent_source_reservations_never_cross_the_ceiling() {
        let total = Arc::new(AtomicUsize::new(0));
        let accepted = (0..16)
            .map(|_| {
                let total = Arc::clone(&total);
                std::thread::spawn(move || try_reserve_source_bytes(&total, 10, 50).is_ok())
            })
            .map(|worker| worker.join().expect("reservation worker should not panic"))
            .filter(|was_accepted| *was_accepted)
            .count();

        assert_eq!(accepted, 5);
        assert_eq!(total.load(Ordering::Relaxed), 50);
    }

    #[test]
    fn metadata_reservation_reconciles_to_the_exact_read_length() {
        let shrank = AtomicUsize::new(20);
        assert_eq!(reconcile_source_bytes(&shrank, 8, 3, 25), Ok(()));
        assert_eq!(shrank.load(Ordering::Relaxed), 15);

        let grew = AtomicUsize::new(20);
        assert_eq!(reconcile_source_bytes(&grew, 8, 10, 25), Ok(()));
        assert_eq!(grew.load(Ordering::Relaxed), 22);

        let rejected_growth = AtomicUsize::new(20);
        assert_eq!(reconcile_source_bytes(&rejected_growth, 8, 14, 25), Err(20));
        assert_eq!(rejected_growth.load(Ordering::Relaxed), 20);
    }
}
