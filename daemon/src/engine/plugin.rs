//! Native Rolldown plugin (spec §7.2/§7.3): serves the virtual entry, maps
//! pre-resolved targets, records loaded real paths, and enforces the product
//! resource limits. It must never override linking or tree-shaking semantics
//! (spec §7.4), so no hook ever returns `HookSideEffects` for a real module.

use std::{
    borrow::Cow,
    collections::HashSet,
    path::PathBuf,
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
use crate::pipeline::graph::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES};

/// Per-build state shared with the adapter, which reads it after the bundler
/// finishes. Limit state is monotonic and thread-safe (spec §7.3).
#[derive(Debug, Default)]
pub(super) struct BuildState {
    loaded_paths: Mutex<HashSet<PathBuf>>,
    total_source_bytes: AtomicUsize,
    limit_breach: Mutex<Option<String>>,
}

impl BuildState {
    /// Canonical form promised by the contract (§5.1): canonicalized to the
    /// same identity form the production fingerprint pipeline uses
    /// (`fs::canonicalize`), sorted, deduplicated. A path that no longer
    /// resolves (deleted mid-build) falls back to the resolver's form.
    pub(super) fn sorted_loaded_paths(&self) -> Vec<PathBuf> {
        let paths = self
            .loaded_paths
            .lock()
            .expect("loaded-path set should not be poisoned");
        let mut sorted: Vec<PathBuf> = paths
            .iter()
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
            .collect();
        sorted.sort();
        sorted.dedup();
        sorted
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

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if args.id == VIRTUAL_ENTRY_ID {
            return Ok(Some(HookLoadOutput {
                code: self.entry_source.as_str().into(),
                module_type: Some(ModuleType::Js),
                ..HookLoadOutput::default()
            }));
        }
        Ok(None)
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

        let module_count = {
            let mut paths = self
                .state
                .loaded_paths
                .lock()
                .expect("loaded-path set should not be poisoned");
            paths.insert(path.to_path_buf());
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
