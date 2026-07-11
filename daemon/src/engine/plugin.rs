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

    fn record_breach(&self, message: &str) {
        let mut breach = self
            .limit_breach
            .lock()
            .expect("limit-breach slot should not be poisoned");
        breach.get_or_insert_with(|| message.to_owned());
    }
}

#[derive(Debug)]
pub(super) struct ImportLensPlugin {
    entry_source: String,
    targets: Vec<PathBuf>,
    state: Arc<BuildState>,
}

impl ImportLensPlugin {
    pub(super) fn for_request(request: &super::BundleRequest) -> Self {
        Self {
            entry_source: super::entry::virtual_entry_source(&request.entries),
            targets: request
                .entries
                .iter()
                .map(|entry| entry.entry_path.clone())
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
            // package specifier.
            return Ok(Some(HookResolveIdOutput::from_id(
                target.to_string_lossy().into_owned(),
            )));
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
        if total_bytes > MAX_GRAPH_SOURCE_BYTES {
            return Err(self
                .breach(format!(
                    "module graph exceeds the {MAX_GRAPH_SOURCE_BYTES} byte total source limit"
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
