//! Native Rolldown plugin (spec §7.2/§7.3): serves the virtual entry, maps
//! pre-resolved targets, records loaded real paths, and enforces the product
//! resource limits. It must never override linking or tree-shaking semantics
//! (spec §7.4), so no hook ever returns `HookSideEffects` for a real module.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
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
    FileFingerprint, absent_file_fingerprint, content_hash, file_fingerprint_from_read_time,
    read_time_len_mtime_of, sort_and_dedup_fingerprints, unverifiable_file_fingerprint,
};

/// Why a classified asset input could not be observed.
///
/// The distinction decides whether the whole result may be cached. A file that is NOT THERE is a
/// deterministic fact about the package that a later freshness probe can confirm. A file that
/// exists but could not be READ is a filesystem moment, and must not be cached as a package fact.
///
/// Absence is common, not exceptional: napi-rs packages `require` one binary per platform triple
/// and ship one, so treating the misses as unreadable would make every such build uncacheable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AssetInputFailure {
    /// Not on disk. Deterministic, and reusable: [`absent_file_fingerprint`] stays Fresh while it
    /// stays missing, and stops being Fresh the moment somebody installs it.
    Absent,
    /// Present but unreadable. Request-local, and never reusable.
    Unreadable,
}

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
    /// [`ImportLensPlugin::load`]; the pipeline processes them and counts their shipped bytes.
    assets: Mutex<HashMap<PathBuf, CollectedAsset>>,
    /// Classified assets this plugin could not observe, and why: the two reasons are cached in
    /// opposite directions, so the reason travels with the path.
    failed_asset_inputs: Mutex<HashMap<PathBuf, AssetInputFailure>>,
    /// Directly imported files that ship but are outside the measured taxonomy (an image, an icon).
    /// Stubbed so they cannot fail the build, and disclosed so their bytes are not silently absent.
    unmeasured_assets: Mutex<BTreeMap<PathBuf, UncountedAsset>>,
    /// Bare specifiers this build turned into an import boundary because the resolver refused them.
    /// Disclosed, never silent: the graph behind such an edge is not in the number.
    unresolved_externals: Mutex<BTreeSet<String>>,
    /// Stable ids of the build's entry modules, spelled exactly as Rolldown's diagnostics name an
    /// importer: the virtual entry for a size build, the real entry for export enumeration.
    entry_stable_ids: Mutex<Vec<String>>,
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

    /// Read-time fingerprints, plus the loaded paths that have none: modules the
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
    /// result. Their bytes are not in the JavaScript chunk but do ship, so the pipeline processes
    /// them the way they ship and folds the result into the size.
    pub(super) fn sorted_assets(&self) -> Vec<CollectedAsset> {
        let assets = self
            .assets
            .lock()
            .expect("asset map should not be poisoned");
        let mut sorted: Vec<CollectedAsset> = assets.values().cloned().collect();
        sorted.sort_by(|left, right| left.path.cmp(&right.path));
        sorted
    }

    /// Returns whether this call claimed the path. `false` means a duplicate hook invocation, whose
    /// byte reservation the caller must release (as with `record_asset`).
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

    /// `Unreadable` always wins over `Absent` for the same path, in either arrival order, so a
    /// filesystem moment never reaches a durable store by ordering luck.
    pub(super) fn record_failed_asset_input(&self, path: PathBuf, failure: AssetInputFailure) {
        let mut inputs = self
            .failed_asset_inputs
            .lock()
            .expect("failed asset-input map should not be poisoned");
        let slot = inputs.entry(path).or_insert(failure);
        if failure == AssetInputFailure::Unreadable {
            *slot = AssetInputFailure::Unreadable;
        }
    }

    fn record_unresolved_external(&self, specifier: String) {
        self.unresolved_externals
            .lock()
            .expect("unresolved-external set should not be poisoned")
            .insert(specifier);
    }

    /// Sorted for a stable disclosure: module order is a concurrency race.
    pub(super) fn unresolved_externals(&self) -> Vec<String> {
        self.unresolved_externals
            .lock()
            .expect("unresolved-external set should not be poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub(super) fn entry_stable_ids(&self) -> Vec<String> {
        self.entry_stable_ids
            .lock()
            .expect("entry stable-id list should not be poisoned")
            .clone()
    }

    /// Only the UNREADABLE ones: this drives the transient `asset_io` diagnostic and failure stage,
    /// and an absent input belongs in neither.
    pub(super) fn unreadable_asset_paths(&self) -> Vec<PathBuf> {
        let mut paths = self
            .failed_asset_inputs
            .lock()
            .expect("failed asset-input map should not be poisoned")
            .iter()
            .filter(|(_, failure)| **failure == AssetInputFailure::Unreadable)
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }

    /// One fingerprint per unobserved input: an absent file stays Fresh while it stays missing (so
    /// the result caches and self-heals on install); an unreadable one is never Fresh (so the
    /// result never enters a durable store).
    pub(super) fn asset_input_fingerprints(&self) -> Vec<FileFingerprint> {
        let mut fingerprints = self
            .failed_asset_inputs
            .lock()
            .expect("failed asset-input map should not be poisoned")
            .iter()
            .map(|(path, failure)| match failure {
                AssetInputFailure::Absent => absent_file_fingerprint(path),
                AssetInputFailure::Unreadable => unverifiable_file_fingerprint(path),
            })
            .collect::<Vec<_>>();
        sort_and_dedup_fingerprints(&mut fingerprints);
        fingerprints
    }

    /// Canonicalize once per build. A path that no longer resolves (deleted
    /// mid-build) falls back to the resolver's form.
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
        // Windows, and holding it would serialize every concurrent module behind one mutex.
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.canonical
            .lock()
            .expect("canonical-path memo should not be poisoned")
            .insert(path.to_path_buf(), canonical.clone());
        canonical
    }

    /// Bind a module id carrying a loader suffix (`./font.woff2?url`) to the file `load` read for
    /// it. `module_parsed` sees only the raw id, and canonicalizing that would key the module under
    /// a path with no read-time fingerprint, which makes the whole result uncacheable.
    fn alias_canonical(&self, id: &Path, canonical: &Path) {
        self.canonical
            .lock()
            .expect("canonical-path memo should not be poisoned")
            .insert(id.to_path_buf(), canonical.to_path_buf());
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
    /// These hooks run on concurrent module tasks, so arrival order is a race, and a
    /// `module_graph_limit` failure is deterministic and cached with its message (ADR-0006).
    /// Ordering by content makes the message a function of the bytes, the same rule
    /// `engine::stage::rank` applies to diagnostics.
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
                path: crate::cache::key::identity_path_string(canonical),
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

/// The module type of a file `load` read under a loader suffix (`./data.json?raw`), named from the
/// file's own extension through Rolldown's type names. Rolldown infers the type from the id it was
/// given, and `json?raw` is no extension it knows, so the JSON would reach the JavaScript parser.
/// `None` when nothing was stripped, or the extension names no Rolldown type (`.mjs`): Rolldown
/// then infers it as it does for any id.
fn loader_suffix_module_type(read: &Path, id: &Path) -> Option<ModuleType> {
    if read == id {
        return None;
    }
    let extension = read.extension()?.to_str()?;
    ModuleType::from_known_str(extension).ok()
}

/// The filesystem-looking portion of a specifier or module id, with any loader suffix removed.
///
/// Shared by `resolve_id` and `load`, so both agree on what a module id like `./font.woff2?url`
/// names.
fn path_portion(specifier: &str) -> &str {
    // A Windows verbatim path (`\\?\C:\...`, what `fs::canonicalize` returns for the whole graph)
    // carries a literal `?` INSIDE its prefix. Skip the prefix, then look for a suffix.
    const VERBATIM_PREFIX: &str = r"\\?\";
    let offset = if specifier.starts_with(VERBATIM_PREFIX) {
        VERBATIM_PREFIX.len()
    } else {
        0
    };
    let scanned = &specifier[offset..];

    let query = scanned.find('?');
    // A leading `#` is a package-import specifier, not a URL fragment. A later `#` is still a
    // loader-style fragment and is removed before extension classification.
    let fragment = if let Some(package_import) = scanned.strip_prefix('#') {
        package_import.find('#').map(|index| index + 1)
    } else {
        scanned.find('#')
    };
    let path_end = query
        .into_iter()
        .chain(fragment)
        .min()
        .unwrap_or(scanned.len());
    &specifier[..offset + path_end]
}

fn supported_asset_observation_candidate(specifier: &str, importer: &str) -> Option<PathBuf> {
    let specifier_path = Path::new(path_portion(specifier));
    classify_asset_class(specifier_path)?;
    if specifier_path.is_absolute() {
        return Some(specifier_path.to_path_buf());
    }
    let is_package_relative = specifier.starts_with("./") || specifier.starts_with("../");
    if !is_package_relative {
        // Bare/self-referential/aliased specifiers have no filesystem candidate until the
        // configured resolver answers, so the spelling stands in; it is never probed or recorded.
        return Some(specifier_path.to_path_buf());
    }
    let importer = Path::new(importer);
    if !importer.is_absolute() {
        return None;
    }
    let parent = importer.parent()?;
    Some(parent.join(specifier_path))
}

/// A specifier that names a subpath of ANOTHER package rather than a file inside this one.
///
/// Path-like specifiers are deliberately excluded: a package that cannot find its own relative file
/// really is broken. A BARE specifier names something across a package boundary, and a boundary we
/// cannot cross is a boundary, not a fatality.
///
/// Rolldown externalizes an unresolvable bare import only when the resolver answers `NotFound`. A
/// file that exists behind an `exports` map answers `PackagePathNotExported` and fails the whole
/// build, typically from a branch that never executes (`jest-resolve/build/defaultResolver`,
/// `eslint/lib/rules`).
///
/// Restricted to subpaths to keep the extra resolver call off the common path: a bare ROOT
/// specifier that is not installed is the `NotFound` case Rolldown already handles.
fn is_bare_subpath_specifier(specifier: &str) -> bool {
    if specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || specifier.starts_with('#')
        || Path::new(specifier).is_absolute()
    {
        return false;
    }
    // `@scope/pkg/sub` needs three segments to be a subpath; `pkg/sub` needs two.
    let required = if specifier.starts_with('@') { 3 } else { 2 };
    specifier.split('/').filter(|part| !part.is_empty()).count() >= required
}

/// A read that failed because the file is not there is a fact about the package; anything else
/// (a permission denial, a locked file, a device error) is a moment on this machine.
fn failure_kind_of(error: &std::io::Error) -> AssetInputFailure {
    if error.kind() == std::io::ErrorKind::NotFound {
        AssetInputFailure::Absent
    } else {
        AssetInputFailure::Unreadable
    }
}

/// How to record an absolute candidate the configured resolver could not answer for.
async fn resolve_failure_kind(candidate: &Path) -> AssetInputFailure {
    match tokio::fs::metadata(candidate).await {
        Err(error) => failure_kind_of(&error),
        // It exists but the resolver still refused it (an `exports` denial, a bad symlink target).
        // Not an absence, so do not claim one.
        Ok(_) => AssetInputFailure::Unreadable,
    }
}

/// Atomically reserve bytes, leaving the counter untouched when the reservation does not fit.
///
/// Not `fetch_add`: it mutates first, so every rejected module would permanently inflate the total
/// and manufacture follow-on breaches.
fn try_reserve_source_bytes(total: &AtomicUsize, bytes: usize, limit: usize) -> Result<(), usize> {
    total
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current
                .checked_add(bytes)
                .filter(|candidate| *candidate <= limit)
        })
        .map(|_| ())
}

fn release_source_bytes(total: &AtomicUsize, bytes: usize) {
    total
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
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
/// Pre-resolving is the point (§6.1): the engine never re-resolves the bare package specifier.
/// Rolldown builds a plugin-resolved `ResolvedId`'s `package_json` from
/// `HookResolveIdOutput::package_json_path` alone, so without it the entry module (and only the
/// entry module) would have no package metadata.
///
/// This is metadata supply, not a semantic override (ADR-0002): Rolldown alone decides what the
/// manifest means, and `side_effects` stays `None` (§7.4).
///
/// **It is the package-ROOT manifest.** That is right for `sideEffects` (read from the package
/// root) but not for `"type"` (read from the *nearest* manifest), so a nested
/// `esm/package.json` `{"type":"module"}` does not reach the entry (known issue C6). Do not supply
/// the nearest manifest instead: that trades a rare format error for a common `sideEffects` error.
///
/// **Both paths must be canonical.** Rolldown matches `sideEffects` globs against the entry id
/// relativized to the manifest's parent, using our strings verbatim. If they do not share a root
/// (a `\\?\` verbatim id against a plain manifest path, or a pnpm store link or workspace junction
/// on one side only), relativization yields the absolute path, and any glob containing a `/` is
/// anchored and can never match it: Rolldown silently tree-shakes side effects the package
/// declared. Fix the input, never the badge.
///
/// The id stays as it is: `entry_path` is canonicalized upstream because read-time
/// fingerprinting, the loaded path set and the module contributions key on it (§8.3).
#[derive(Debug)]
struct PreResolvedTarget {
    entry_path: PathBuf,
    /// The **canonical** `<package_root>/package.json`, or `None` when there is none to point at.
    ///
    /// Rolldown *reads* this path, and an unreadable one fails the whole build. A `BundleEntry`
    /// does not promise its `package_root` holds a manifest (the qualification fixtures point at
    /// bare directories).
    manifest_path: Option<String>,
}

impl PreResolvedTarget {
    fn for_entry(entry: &super::BundleEntry) -> Self {
        Self {
            // Canonical on both sides (see above): `BundleEntry` promises only an absolute entry,
            // not a canonical one. Idempotent for the paths that already are canonical.
            entry_path: std::fs::canonicalize(&entry.entry_path)
                .unwrap_or_else(|_| entry.entry_path.clone()),
            manifest_path: canonical_manifest_path(&entry.package_root)
                .map(|manifest| manifest.to_string_lossy().into_owned()),
        }
    }
}

/// The package manifest, spelled the way the entry id is spelled: canonical.
///
/// `canonicalize` proves it exists and resolves the links (see [`PreResolvedTarget`]). The
/// `is_file` check is still needed: a *directory* named `package.json` canonicalizes too, and
/// handing Rolldown a directory to read fails the entire build.
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
    /// `entries[i]` and `resolve_id` maps it back with `targets.get(i)`. A file-size build mixes
    /// entries from different packages, so any reordering applies one package's manifest to
    /// another's entry. Never sort, dedup or filter this vector (row 51 of
    /// `tests/candidate_matrix.rs` guards it).
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

    /// A bare specifier the resolver refused. A subpath of another package becomes a disclosed
    /// import boundary (see [`is_bare_subpath_specifier`]); anything else is left to Rolldown,
    /// which externalizes a `NotFound` root with a warning and fails the rest.
    fn unresolved_boundary(&self, specifier: &str) -> Option<HookResolveIdOutput> {
        if !is_bare_subpath_specifier(specifier) {
            return None;
        }
        self.state.record_unresolved_external(specifier.to_owned());
        Some(HookResolveIdOutput {
            external: Some(true.into()),
            ..HookResolveIdOutput::from_id(specifier.to_owned())
        })
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
            // Pre-resolved absolute path (§6.1), plus the package manifest Rolldown would have
            // found on the way (see [`PreResolvedTarget`]).
            return Ok(Some(HookResolveIdOutput {
                package_json_path: target.manifest_path.clone(),
                ..HookResolveIdOutput::from_id(target.entry_path.to_string_lossy().into_owned())
            }));
        }
        if let Some(importer) = args.importer
            && let Some(candidate) = supported_asset_observation_candidate(args.specifier, importer)
        {
            // Ask Rolldown's configured resolver (with this hook skipped) rather than joining a
            // relative path ourselves: Client/Component builds apply package `browser` aliases,
            // and a raw join would measure the server asset or ignore a `false` mapping.
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
                    // A bare, self-referential or aliased spelling has no filesystem location to
                    // probe, so its failure is the resolver's deterministic verdict; it takes the
                    // same boundary path as any other refused bare specifier.
                    if !candidate.is_absolute() {
                        return Ok(self.unresolved_boundary(args.specifier));
                    }
                    // Most misses here are a package fact (napi-rs platform probes), not a
                    // filesystem hiccup; recording which lets a correct build still be cached.
                    let failure = resolve_failure_kind(&candidate).await;
                    self.state.record_failed_asset_input(candidate, failure);
                    // Let the normal resolver run once more so Rolldown retains its native resolve
                    // diagnostic. `classify_failure` promotes our typed `asset_io` observation.
                    return Ok(None);
                }
            }
        }

        // A cross-package subpath the resolver refuses is an import BOUNDARY, externalized and
        // recorded for disclosure (see [`is_bare_subpath_specifier`]).
        if let Some(importer) = args.importer
            && is_bare_subpath_specifier(args.specifier)
        {
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
            return match resolved {
                // Hand back the id we already paid for rather than returning `None` and making
                // Rolldown resolve the same specifier a second time.
                Ok(resolved) => Ok(Some(HookResolveIdOutput::from_resolved_id(resolved))),
                Err(_) => Ok(self.unresolved_boundary(args.specifier)),
            };
        }
        Ok(None)
    }

    /// Reads real modules itself so their bytes are fingerprinted at the moment they are
    /// consumed (§8.3). Re-reading after the build would record a file edited mid-analysis with
    /// its NEW bytes against a size measured from the OLD ones, and that entry would probe
    /// `Fresh` forever.
    ///
    /// The bytes are hashed raw, before any transform, so a `.ts` module hashes to its on-disk
    /// content, which is what a later probe compares against.
    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if args.id == VIRTUAL_ENTRY_ID {
            return Ok(Some(HookLoadOutput {
                code: self.entry_source.as_str().into(),
                module_type: Some(ModuleType::Js),
                ..HookLoadOutput::default()
            }));
        }

        // Real module ids are absolute paths; synthetic ids are left to Rolldown. An id can carry
        // a loader suffix (`./font.woff2?url`, `./data.json?raw`) that oxc_resolver re-appends,
        // so the raw id may name no file.
        let literal = Path::new(args.id);
        let stripped = Path::new(path_portion(args.id));
        if !stripped.is_absolute() {
            return Ok(None);
        }

        // Resolve the identity BEFORE the stat/read pair: canonicalizing after the read can pair
        // bytes from an old symlink target with the path of a newly-retargeted one.
        //
        // Strip to rescue a loader suffix, never to lose a real file: `?` is legal in a Linux
        // filename and `#` everywhere, so a stripped path not on disk falls back to the literal
        // id. Whichever file is chosen is the module's identity for `module_parsed` too.
        let mut path = stripped;
        let mut canonical = self.state.canonical_path(stripped);
        let mut stat = tokio::fs::metadata(&canonical).await;
        if stat.is_err() && literal != stripped {
            let literal_canonical = self.state.canonical_path(literal);
            if let Ok(metadata) = tokio::fs::metadata(&literal_canonical).await {
                path = literal;
                canonical = literal_canonical;
                stat = Ok(metadata);
            }
        }
        if stat.is_ok() && path != literal {
            self.state.alias_canonical(literal, &canonical);
        }

        let asset_class = classify_asset_class(path);
        let asset_kind = match asset_class {
            Some(AssetClass::Counted(kind)) => Some(kind),
            _ => None,
        };

        let metadata = match stat {
            Ok(metadata) => metadata,
            Err(error) => {
                if asset_class.is_some() {
                    let failure = failure_kind_of(&error);
                    self.state.record_failed_asset_input(canonical, failure);
                    // Do not let the default loader reopen the asset outside this plugin's
                    // source-byte reservations; the adapter reports the retained cause.
                    return Err(error.into());
                }
                return Ok(None);
            }
        };
        // §7.3: reject an oversized module BEFORE reading it, or reading would blow the memory
        // bound being enforced. `module_parsed` also enforces it on the transformed source,
        // covering modules this hook hands back to Rolldown.
        if metadata.len() > MAX_MODULE_SOURCE_BYTES as u64 {
            self.state.record_stat_fingerprint(&canonical, &metadata);
            return Err(self
                .breach(format!(
                    "module {} exceeds the {MAX_MODULE_SOURCE_BYTES} byte module source limit",
                    canonical.display()
                ))
                .into());
        }

        // len+mtime come from the stat taken BEFORE the read. Stat-after-read would pair
        // post-edit metadata with a hash of pre-edit bytes, and the freshness fast path matches
        // on len+mtime alone, so a file rewritten during the read would probe Fresh forever.
        let (len, modified_millis) = read_time_len_mtime_of(&metadata);

        // Direct assets become empty Rolldown modules, so `module_parsed` sees zero bytes for them.
        // Reserve the stat length BEFORE reading, so the aggregate cap covers them without first
        // allocating past it. The per-file check above makes the `usize` conversion safe.
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
                    let failure = failure_kind_of(&error);
                    self.state.record_failed_asset_input(canonical, failure);
                    return Err(error.into());
                }
                return Ok(None);
            }
        };

        // A counted non-JavaScript ASSET, intercepted BEFORE the UTF-8 conversion: a wasm or font
        // handed back to Rolldown can perturb or fail the JS build, and a stylesheet fails it
        // outright (`UNSUPPORTED_FEATURE`).
        //
        // `ModuleType::Empty` links it as nothing (shimming any imported binding), so the JS graph
        // measures exactly; the asset is recorded with its kind and the pipeline counts the bytes
        // it ships.
        if let Some(kind) = asset_kind {
            let reserved = reserved_asset_bytes
                .expect("a classified asset must reserve its metadata length before reading");
            let asset = CollectedAsset::from_read(canonical, kind, &metadata, bytes);
            let actual = asset.bytes().len();

            // A file may grow between metadata and read: this post-read check closes that gap and
            // fingerprints the bytes that made the deterministic failure true.
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

        // A file that ships but is outside the measured taxonomy (an image, an icon, a media file,
        // a native `.node` addon). Left to Rolldown, one of these fails the whole build: a binary
        // fails its loader, and an `.svg` is parsed as JavaScript.
        //
        // Stubbed to `Empty` so the JS graph measures exactly, and DISCLOSED: the size omits
        // bytes that ship, so it is a floor. Its length is still charged against the aggregate
        // ceiling, so stubbing cannot admit bytes no limit sees.
        if asset_class == Some(AssetClass::Unmeasured) {
            let reserved = reserved_asset_bytes
                .expect("a classified asset must reserve its metadata length before reading");
            let actual = bytes.len();

            // Same post-read growth check as the counted arm.
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
                // Fingerprint before returning: this failure is deterministic and cached, and must
                // expire when the file that caused it changes.
                self.record_read_time(&canonical, len, modified_millis, &bytes);
                self.release_source_bytes(reserved);
                return Err(error.into());
            }
            self.record_read_time(&canonical, len, modified_millis, &bytes);

            // Release on a DUPLICATE, as the counted arm does: two module ids can canonicalize to
            // one path (pnpm symlinks), and only the first is accounted for.
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

        // Hash BEFORE the UTF-8 conversion, so the conversion can consume the buffer without a
        // graph-sized copy.
        self.record_read_time(&canonical, len, modified_millis, &bytes);

        // A binary module that is NOT a classified asset. Rolldown handles those itself; the caller
        // back-fills their fingerprints from `read_time_fingerprints`.
        let Ok(source) = String::from_utf8(bytes) else {
            return Ok(None);
        };

        Ok(Some(HookLoadOutput {
            code: source.into(),
            module_type: loader_suffix_module_type(path, literal),
            ..HookLoadOutput::default()
        }))
    }

    async fn module_parsed(
        &self,
        _ctx: &PluginContext,
        module_info: Arc<ModuleInfo>,
        normal_module: &NormalModule,
    ) -> HookNoopReturn {
        if module_info.is_entry {
            self.state
                .entry_stable_ids
                .lock()
                .expect("entry stable-id list should not be poisoned")
                .push(normal_module.stable_id.to_string());
        }
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

    /// A Windows verbatim path carries a literal `?` inside its prefix, and `fs::canonicalize`
    /// returns that form for the whole module graph. The suffix cases are what the helper is FOR;
    /// the verbatim case is what it must not eat.
    #[test]
    fn path_portion_strips_a_loader_suffix_without_eating_a_verbatim_prefix() {
        assert_eq!(
            path_portion(r"\\?\C:\pkg\node_modules\lib\index.js"),
            r"\\?\C:\pkg\node_modules\lib\index.js",
            "the `?` in a verbatim prefix is part of the path, not a query"
        );
        assert_eq!(
            path_portion(r"\\?\C:\pkg\font.woff2?url"),
            r"\\?\C:\pkg\font.woff2",
            "a real suffix is still stripped from a verbatim path"
        );
        assert_eq!(path_portion("./font.woff2?url"), "./font.woff2");
        assert_eq!(path_portion("./data.json?raw"), "./data.json");
        assert_eq!(path_portion("./icon.svg#iefix"), "./icon.svg");
        assert_eq!(path_portion("./plain.js"), "./plain.js");
        assert_eq!(
            path_portion("#font.woff2"),
            "#font.woff2",
            "a leading `#` is a package-import specifier, not a fragment"
        );
    }

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
        state.record_failed_asset_input(failed_read.clone(), AssetInputFailure::Unreadable);

        let observations = state.asset_input_fingerprints();
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

    /// The other half of the same rule: a file that is simply NOT THERE is a deterministic fact,
    /// so it must neither refuse the cache nor claim the filesystem needs to settle.
    #[test]
    fn an_absent_asset_observation_is_reusable_and_is_not_an_io_failure() {
        let state = BuildState::default();
        let missing = PathBuf::from("/pkg/crc32.darwin-arm64.node");
        state.record_failed_asset_input(missing.clone(), AssetInputFailure::Absent);

        let observations = state.asset_input_fingerprints();
        assert_eq!(observations.len(), 1);
        assert!(
            crate::cache::key::fingerprint_is_absent(&observations[0]),
            "an absent input must record the absence it can later re-confirm"
        );
        assert!(
            !crate::cache::key::fingerprint_is_unverifiable(&observations[0]),
            "an absence is not a machine-dependent read failure"
        );
        assert!(
            crate::cache::key::fingerprints_are_reusable(&observations),
            "a deterministic absence must not refuse the result it belongs to"
        );
        assert!(
            state.unreadable_asset_paths().is_empty(),
            "an absent input must not raise the transient asset_io diagnostic"
        );
    }

    /// Ordering must not decide durability. Two imports of the same path can reach these hooks
    /// concurrently, and the stricter observation has to survive whichever lands second.
    #[test]
    fn an_unreadable_observation_outranks_an_absent_one_in_either_order() {
        for absent_first in [true, false] {
            let state = BuildState::default();
            let path = PathBuf::from("/pkg/contested.node");
            let order = if absent_first {
                [AssetInputFailure::Absent, AssetInputFailure::Unreadable]
            } else {
                [AssetInputFailure::Unreadable, AssetInputFailure::Absent]
            };
            for failure in order {
                state.record_failed_asset_input(path.clone(), failure);
            }

            let observations = state.asset_input_fingerprints();
            assert_eq!(observations.len(), 1);
            assert!(
                crate::cache::key::fingerprint_is_unverifiable(&observations[0]),
                "unreadable must win regardless of arrival order (absent_first={absent_first})"
            );
        }
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
