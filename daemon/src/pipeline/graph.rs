use crate::{
    cache::key::{FileFingerprint, Freshness, check_fingerprints_strict},
    ipc::protocol::ImportRuntime,
    pipeline::resolver::{
        ResolverSet, normalize_existing_path, resolve_module_path, shared_resolvers,
    },
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentTargetPropertyIdentifier, BindingPattern, BindingProperty, Declaration,
    ExportDefaultDeclarationKind, Expression, ObjectProperty, Program, Statement,
    StaticMemberExpression,
};
use oxc_ast_visit::{Visit, walk};
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_resolver::Resolver;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::module_record::{
    ExportEntry, ExportExportName, ExportImportName, ExportLocalName, ImportEntry,
    ImportImportName, ModuleRecord as OxcModuleRecord,
};
use oxc_transformer::{TransformOptions, Transformer};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static GRAPH_CACHE: OnceLock<papaya::HashMap<(PathBuf, ImportRuntime), CachedModuleGraph>> =
    OnceLock::new();

pub const MAX_GRAPH_MODULES: usize = 2_000;
pub const MAX_MODULE_SOURCE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_GRAPH_SOURCE_BYTES: usize = 100 * 1024 * 1024;
// Every cached graph retains the full prepared source of all its modules, so
// an unbounded cache can hold gigabytes across a long multi-package session.
pub const MAX_CACHED_GRAPHS: usize = 32;

#[derive(Debug, Clone)]
struct CachedModuleGraph {
    graph: Arc<ModuleGraph>,
    fingerprints: Vec<FileFingerprint>,
    last_used_millis: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphLimits {
    pub max_modules: usize,
    pub max_module_source_bytes: usize,
    pub max_graph_source_bytes: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            max_modules: MAX_GRAPH_MODULES,
            max_module_source_bytes: MAX_MODULE_SOURCE_BYTES,
            max_graph_source_bytes: MAX_GRAPH_SOURCE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone)]
pub struct ModuleRecord {
    pub id: ModuleId,
    pub path: PathBuf,
    pub source: String,
    pub original_source_bytes: usize,
    /// xxh3 of the raw file bytes read for this module (pre-transform). Lets the
    /// cache detect real content changes without re-reading on a warm graph.
    pub content_hash: u64,
    /// File length + mtime captured immediately BEFORE the source read (never
    /// after). A change landing between the stat and the read then mismatches the
    /// stored len/mtime, so the freshness check falls through to `content_hash`
    /// (which describes the bytes actually analyzed). Capturing after the read
    /// would pair fresh len/mtime with a stale hash and let the mtime+len
    /// pre-filter serve a stale analysis as Fresh indefinitely.
    pub content_len: u64,
    pub content_mtime_millis: u64,
    pub imports: Vec<ImportEdge>,
    pub external_imports: Vec<ExternalImportEdge>,
    pub import_statement_spans: Vec<(usize, usize)>,
    pub export_specifier_statement_spans: Vec<(usize, usize)>,
    pub exports: Vec<ExportRecord>,
    pub reexports: Vec<ReExportRecord>,
    pub star_exports: Vec<StarExportRecord>,
    pub local_bindings: Vec<String>,
    pub binding_dependencies: Vec<BindingDependencyRecord>,
    /// Root-scope bindings referenced by top-level statements that declare
    /// nothing (`setup(dep);`). The rewriter keeps such statements verbatim and
    /// the minifier cannot prove them side-effect free, so the names they read
    /// are retention roots: prune them and the bundle references an undeclared
    /// binding.
    pub side_effect_references: Vec<String>,
    /// Every non-computed, non-optional `identifier.property` access in the
    /// module. `object` is the identifier's own span so the bundler can match it
    /// against `root_symbol_spans` and thereby ignore shadowed uses.
    pub static_member_accesses: Vec<StaticMemberAccess>,
    // Root-scope symbol declaration + reference spans, computed once here so the
    // bundle rewriter does not re-parse and re-run semantic analysis per request.
    pub root_symbol_spans: Vec<RootSymbolSpans>,
    pub shorthand_spans: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct StaticMemberAccess {
    /// Span of the object identifier alone (`ns` in `ns.alpha`). Matches a span
    /// in `RootSymbolSpans::references`.
    pub object: (usize, usize),
    /// Span of the whole member expression (`ns.alpha`), i.e. what a rewrite
    /// replaces.
    pub span: (usize, usize),
    pub property: String,
}

#[derive(Debug, Clone)]
pub struct RootSymbolSpans {
    pub name: String,
    pub decl: (usize, usize),
    pub references: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct GraphDiagnostic {
    pub stage: String,
    pub message: String,
    pub details: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImportEdge {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_names: Vec<String>,
    pub imported_bindings: Vec<ImportedBinding>,
}

#[derive(Debug, Clone)]
pub struct ExternalImportEdge {
    pub specifier: String,
    pub imported_name: String,
    pub local_name: String,
}

#[derive(Debug, Clone)]
pub struct ImportedBinding {
    pub imported_name: String,
    pub local_name: String,
}

#[derive(Debug, Clone)]
pub struct BindingDependencyRecord {
    pub binding_name: String,
    pub referenced_name: String,
}

#[derive(Debug, Clone)]
pub struct ExportRecord {
    pub exported_name: String,
    pub local_name: String,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct ReExportRecord {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_name: String,
    pub exported_name: String,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct StarExportRecord {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct ModuleGraph {
    pub entry_id: ModuleId,
    pub modules: Vec<ModuleRecord>,
    pub diagnostics: Vec<GraphDiagnostic>,
    pub dependency_paths: Vec<PathBuf>,
    path_to_id: HashMap<PathBuf, ModuleId>,
    full_bundle_minified_len: OnceLock<Option<u64>>,
}

impl Default for ModuleGraph {
    fn default() -> Self {
        Self {
            entry_id: ModuleId(0),
            modules: Vec::new(),
            diagnostics: Vec::new(),
            dependency_paths: Vec::new(),
            path_to_id: HashMap::new(),
            full_bundle_minified_len: OnceLock::new(),
        }
    }
}

impl ModuleGraph {
    pub fn from_parts(
        entry_id: ModuleId,
        modules: Vec<ModuleRecord>,
        diagnostics: Vec<GraphDiagnostic>,
        dependency_paths: Vec<PathBuf>,
    ) -> Self {
        let path_to_id = modules
            .iter()
            .map(|module| (module.path.clone(), module.id))
            .collect();

        Self {
            entry_id,
            modules,
            diagnostics,
            dependency_paths,
            path_to_id,
            full_bundle_minified_len: OnceLock::new(),
        }
    }

    pub fn module_by_id(&self, id: ModuleId) -> Option<&ModuleRecord> {
        self.modules.get(id.0)
    }

    pub fn module_id_by_path(&self, path: &Path) -> Option<ModuleId> {
        self.path_to_id.get(path).copied()
    }

    /// Raw-source content hash for a loaded module path, if present in the graph.
    /// Canonicalizes first: the graph keys paths canonically (on Windows the
    /// verbatim `\\?\C:\...` form via `fs::canonicalize`), so a raw caller path
    /// must be normalized to match — this also hardens the `dependency_fingerprints`
    /// callers below, which pass raw `package_root.join(..)` / `entry_path` values.
    pub fn content_hash_for(&self, path: &Path) -> Option<u64> {
        let canonical = std::fs::canonicalize(path).ok()?;
        self.module_id_by_path(&canonical)
            .and_then(|id| self.module_by_id(id))
            .map(|module| module.content_hash)
    }

    /// Full read-time fingerprint (len+mtime+hash captured when the module was read)
    /// for a loaded module path, built WITHOUT re-stat'ing for len/mtime. Returns
    /// `None` for a path that is not a loaded graph module (the caller then falls back
    /// to a stat-only fingerprint).
    ///
    /// The graph keys every module by its canonical path (`normalize_existing_path`
    /// = `fs::canonicalize`), and the fingerprint callers pass those canonical keys
    /// directly (`graph.modules[*].path`, resolver-produced `dependency_paths`, and
    /// the already-canonicalized entry). So the RAW path is looked up against the keys
    /// FIRST and only `fs::canonicalize`d on a miss — collapsing the per-module
    /// canonicalize syscall (one per path, ≈N per graph) down to O(non-module paths).
    /// A raw hit and the canonicalize-then-hit return the same module, since
    /// `fs::canonicalize` is idempotent on a path that is already a canonical key.
    /// `module.path` is itself that canonical key, so `file_fingerprint_from_read_time`
    /// forward-slashes it directly without re-canonicalizing (a second per-module
    /// syscall removed). `content_hash_for` still canonicalizes: its sole caller (a
    /// test) passes a raw, non-canonical path.
    pub fn module_fingerprint_for(&self, path: &Path) -> Option<FileFingerprint> {
        let module = match self.module_id_by_path(path) {
            Some(id) => self.module_by_id(id)?,
            None => {
                #[cfg(test)]
                canonicalize_probe::record();
                let canonical = std::fs::canonicalize(path).ok()?;
                self.module_id_by_path(&canonical)
                    .and_then(|id| self.module_by_id(id))?
            }
        };
        Some(crate::cache::key::file_fingerprint_from_read_time(
            &module.path,
            module.content_len,
            module.content_mtime_millis,
            module.content_hash,
        ))
    }

    pub fn cached_full_bundle_minified_len_or_init(
        &self,
        init: impl FnOnce() -> Option<u64>,
    ) -> Option<u64> {
        *self.full_bundle_minified_len.get_or_init(init)
    }

    pub fn cache_full_bundle_minified_len(&self, len: u64) {
        let _ = self.full_bundle_minified_len.set(Some(len));
    }
}

pub fn build_module_graph(entry_path: &Path) -> Result<ModuleGraph, String> {
    build_module_graph_with_runtime(entry_path, ImportRuntime::Component)
}

pub fn build_module_graph_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<ModuleGraph, String> {
    build_module_graph_with_limits_and_runtime(entry_path, GraphLimits::default(), runtime)
}

pub fn build_module_graph_cached(entry_path: &Path) -> Result<Arc<ModuleGraph>, String> {
    build_module_graph_cached_with_runtime(entry_path, ImportRuntime::Component)
}

/// Test-only probe that counts calls to `build_module_graph_cached_with_runtime`
/// for a single armed entry path. The count is recorded before the cache lookup,
/// so cache hits are counted too. Scoping to one path keeps sibling unit tests
/// that build unrelated graphs in the same test binary from perturbing the
/// count. Task A4's regression test uses this seam to prove `analyze_and_cache`
/// fetches the analyzed graph exactly once (reusing it for fingerprints) instead
/// of re-fetching it — the TOCTOU that Finding 4 describes.
#[cfg(test)]
pub mod graph_fetch_probe {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    // (armed_entry, hits). `None` = disarmed; fetches are ignored.
    static STATE: Mutex<Option<(PathBuf, usize)>> = Mutex::new(None);

    fn lock() -> std::sync::MutexGuard<'static, Option<(PathBuf, usize)>> {
        STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Begin counting fetches for `entry`, resetting the count to zero. Only one
    /// path is armed at a time, so probe tests must not run concurrently with
    /// one another (there is a single such test).
    pub fn arm(entry: PathBuf) {
        *lock() = Some((entry, 0));
    }

    /// Number of fetches recorded for the armed path since `arm`.
    pub fn hits() -> usize {
        lock().as_ref().map(|(_, count)| *count).unwrap_or(0)
    }

    /// Stop counting; subsequent fetches are ignored.
    pub fn disarm() {
        *lock() = None;
    }

    pub(super) fn record(entry: &Path) {
        if let Some((watched, count)) = lock().as_mut()
            && watched.as_path() == entry
        {
            *count += 1;
        }
    }
}

/// Test-only probe that counts the `fs::canonicalize` syscalls
/// `module_fingerprint_for` performs while building a graph's fingerprints. Task
/// C8's regression test uses it to prove fingerprint-building is O(non-module
/// paths) canonicalize calls, not O(modules): the module/dep paths are already the
/// graph's canonical keys and must be looked up WITHOUT re-canonicalizing. The
/// counter is thread-local so the synchronous fingerprint call under test is
/// isolated from unrelated graphs built concurrently on other test threads.
#[cfg(test)]
pub mod canonicalize_probe {
    use std::cell::Cell;

    thread_local! {
        // `Some(count)` = armed on this thread; `None` = disarmed.
        static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Begin counting on the current thread, resetting the count to zero.
    pub fn arm() {
        COUNT.with(|count| count.set(Some(0)));
    }

    /// Canonicalize calls recorded on the current thread since `arm`.
    pub fn hits() -> usize {
        COUNT.with(|count| count.get().unwrap_or(0))
    }

    /// Stop counting on the current thread; subsequent calls are ignored.
    pub fn disarm() {
        COUNT.with(|count| count.set(None));
    }

    pub(super) fn record() {
        COUNT.with(|count| {
            if let Some(current) = count.get() {
                count.set(Some(current + 1));
            }
        });
    }
}

pub fn build_module_graph_cached_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<Arc<ModuleGraph>, String> {
    #[cfg(test)]
    graph_fetch_probe::record(entry_path);

    let entry_path = normalize_existing_path(entry_path)?;
    let cache = GRAPH_CACHE.get_or_init(papaya::HashMap::new);
    let pinned = cache.pin();
    let cache_key = (entry_path.clone(), runtime);
    if let Some(graph) = pinned.get(&cache_key) {
        // Strict gate (RB-1 / X-7): hash-verify first-party modules so an
        // equal-length, mtime-preserving rewrite is detected — the non-strict
        // mtime+len pre-filter would reuse the STALE graph here, and since L2
        // recomputes THROUGH this cache, a first-party edit would be served stale
        // forever. Keep serving on a transient `Unknown` (a momentarily-locked
        // file), matching L2's SWR; only a definite `Stale`/`Gone` rebuilds.
        match check_fingerprints_strict(&graph.fingerprints) {
            Freshness::Fresh | Freshness::Unknown => {
                graph
                    .last_used_millis
                    .store(crate::time::unix_millis_now(), Ordering::Relaxed);
                return Ok(Arc::clone(&graph.graph));
            }
            Freshness::Stale | Freshness::Gone => {
                pinned.remove(&cache_key);
            }
        }
    }

    let graph = Arc::new(build_module_graph_with_runtime(&entry_path, runtime)?);
    pinned.insert(
        cache_key,
        CachedModuleGraph {
            fingerprints: module_graph_fingerprints(&entry_path, &graph),
            graph: Arc::clone(&graph),
            last_used_millis: Arc::new(AtomicU64::new(crate::time::unix_millis_now())),
        },
    );

    if pinned.len() > MAX_CACHED_GRAPHS {
        let oldest = pinned
            .iter()
            .min_by_key(|(_, cached)| cached.last_used_millis.load(Ordering::Relaxed))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            pinned.remove(&key);
        }
    }

    Ok(graph)
}

/// Drops cached module graphs whose entry-path key no longer exists on disk
/// (uninstalled package). Used by the orphan purge. Returns the number removed.
pub fn purge_missing_module_graphs() -> usize {
    let Some(cache) = GRAPH_CACHE.get() else {
        return 0;
    };
    let pinned = cache.pin();
    let missing = pinned
        .iter()
        .filter(|((entry_path, _runtime), _)| {
            crate::cache::key::path_is_definitely_gone(entry_path)
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in &missing {
        pinned.remove(key);
    }
    missing.len()
}

/// Returns the module graph for `entry_path` only if it is already cached and
/// still current; never builds one. Used where building would be wasteful, e.g.
/// a prewarm enumeration over a large manifest that would otherwise serialize
/// graph builds and thrash the bounded cache.
pub fn cached_module_graph_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Option<Arc<ModuleGraph>> {
    let entry_path = normalize_existing_path(entry_path).ok()?;
    let cache = GRAPH_CACHE.get()?;
    let pinned = cache.pin();
    let cached = pinned.get(&(entry_path, runtime))?;
    // Strict gate (RB-1 / X-7), same as the building path: hash-verify first-party
    // modules; keep serving on a transient `Unknown`, decline on `Stale`/`Gone`.
    match check_fingerprints_strict(&cached.fingerprints) {
        Freshness::Fresh | Freshness::Unknown => {
            cached
                .last_used_millis
                .store(crate::time::unix_millis_now(), Ordering::Relaxed);
            Some(Arc::clone(&cached.graph))
        }
        Freshness::Stale | Freshness::Gone => None,
    }
}

/// Returns the cached graph's module paths for `(entry, runtime)` WITHOUT the
/// `fingerprints_are_current` freshness gate. The L1 file-size signature re-stats
/// each path itself, so it needs the raw file set, not a validated graph — and a
/// gated accessor would return `None` exactly when a module changed, hiding the very
/// edit L1 must react to. Never builds; leaves `last_used_millis` untouched (a cheap
/// signature peek is not a real consumption that should reshape the LRU). `None` if
/// nothing is cached for the key.
pub fn peek_cached_module_paths(entry_path: &Path, runtime: ImportRuntime) -> Option<Vec<PathBuf>> {
    let entry_path = normalize_existing_path(entry_path).ok()?;
    let cache = GRAPH_CACHE.get()?;
    let pinned = cache.pin();
    let cached = pinned.get(&(entry_path, runtime))?;
    Some(
        cached
            .graph
            .modules
            .iter()
            .map(|module| module.path.clone())
            .collect(),
    )
}

pub fn module_graph_cache_len() -> usize {
    GRAPH_CACHE
        .get()
        .map(|cache| cache.pin().len())
        .unwrap_or(0)
}

fn module_graph_fingerprints(entry_path: &Path, graph: &ModuleGraph) -> Vec<FileFingerprint> {
    let mut paths = Vec::with_capacity(graph.dependency_paths.len() + 1);
    paths.push(entry_path.to_path_buf());
    paths.extend(graph.dependency_paths.iter().cloned());
    fingerprints_with_content_hashes(paths, graph)
}

/// Build content-hash-aware fingerprints for a set of paths, deduped by their
/// normalized `.path` AFTER fingerprinting (so raw and canonical spellings of
/// the same file collapse, matching the pre-redesign `fingerprints_for_paths`).
pub fn fingerprints_with_content_hashes(
    paths: Vec<PathBuf>,
    graph: &ModuleGraph,
) -> Vec<FileFingerprint> {
    let mut fingerprints: Vec<FileFingerprint> = paths
        .into_iter()
        .filter_map(|path| {
            // Loaded modules use their read-time len+mtime+hash (no post-analysis
            // re-stat); non-module paths (e.g. package.json) are read+hashed here
            // (RB-2) so an equal-length, mtime-preserving change is still detected.
            graph
                .module_fingerprint_for(&path)
                .or_else(|| crate::cache::key::file_fingerprint_reading_hash(&path))
        })
        .collect();
    fingerprints.sort_by(|a, b| a.path.cmp(&b.path));
    fingerprints.dedup_by(|a, b| a.path == b.path);
    fingerprints
}

pub fn invalidate_module_graph_cache_for_package(package_name: &str) {
    let Some(cache) = GRAPH_CACHE.get() else {
        return;
    };

    let package_segment = format!("node_modules/{package_name}/");
    let pinned = cache.pin();
    let keys = pinned
        .iter()
        .filter(|((path, _runtime), _)| {
            path.to_string_lossy()
                .replace('\\', "/")
                .contains(&package_segment)
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    for key in keys {
        pinned.remove(&key);
    }
}

pub fn clear_module_graph_cache() {
    if let Some(cache) = GRAPH_CACHE.get() {
        cache.pin().clear();
    }
}

pub fn build_module_graph_with_limits(
    entry_path: &Path,
    limits: GraphLimits,
) -> Result<ModuleGraph, String> {
    build_module_graph_with_limits_and_runtime(entry_path, limits, ImportRuntime::Component)
}

pub fn build_module_graph_with_limits_and_runtime(
    entry_path: &Path,
    limits: GraphLimits,
    runtime: ImportRuntime,
) -> Result<ModuleGraph, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    let mut builder = ModuleGraphBuilder::new(limits, runtime);
    let entry_id = builder.load_module(&entry_path)?;
    builder.graph.entry_id = entry_id;
    builder.graph.dependency_paths = builder.dependency_paths.into_iter().collect();
    builder.graph.dependency_paths.sort();

    Ok(builder.graph)
}

struct ModuleGraphBuilder {
    graph: ModuleGraph,
    limits: GraphLimits,
    graph_source_bytes: usize,
    resolvers: Arc<ResolverSet>,
    runtime: ImportRuntime,
    dependency_paths: HashSet<PathBuf>,
    circular_edges: HashSet<(PathBuf, PathBuf)>,
    loading_paths: HashSet<PathBuf>,
}

impl ModuleGraphBuilder {
    fn new(limits: GraphLimits, runtime: ImportRuntime) -> Self {
        Self {
            graph: ModuleGraph::default(),
            limits,
            graph_source_bytes: 0,
            resolvers: shared_resolvers(),
            runtime,
            dependency_paths: HashSet::new(),
            circular_edges: HashSet::new(),
            loading_paths: HashSet::new(),
        }
    }
}

impl ModuleGraphBuilder {
    fn load_module(&mut self, path: &Path) -> Result<ModuleId, String> {
        self.load_module_from(path, None)
    }

    fn load_module_from(
        &mut self,
        path: &Path,
        importer: Option<&Path>,
    ) -> Result<ModuleId, String> {
        let path = normalize_existing_path(path)?;
        if let Some(existing) = self.graph.path_to_id.get(&path) {
            if self.loading_paths.contains(&path)
                && let Some(importer) = importer
                && self
                    .circular_edges
                    .insert((importer.to_path_buf(), path.clone()))
            {
                self.graph.diagnostics.push(GraphDiagnostic {
                    stage: "circular_dependency".to_owned(),
                    message: "circular module dependency detected".to_owned(),
                    details: vec![
                        format!("from_path: {}", importer.display()),
                        format!("to_path: {}", path.display()),
                    ],
                });
            }
            return Ok(*existing);
        }
        if self.graph.modules.len() >= self.limits.max_modules {
            return Err(format!(
                "module count limit exceeded while loading {}; limit: {}",
                path.display(),
                self.limits.max_modules
            ));
        }

        // Stat BEFORE reading: if the file changes between the stat and the read,
        // the stored fingerprint's len/mtime mismatch the live file and the check
        // falls through to the content hash, which matches the bytes actually
        // analyzed — correct. The reverse order (read, then stat) would pair fresh
        // len/mtime with a stale hash, and the len+mtime pre-filter would then
        // serve the stale analysis as Fresh indefinitely.
        let (content_len, content_mtime_millis) = crate::cache::key::read_time_len_mtime(&path);
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read module {}: {error}", path.display()))?;
        let content_hash = crate::cache::key::content_hash(source.as_bytes());
        let source_bytes = source.len();
        if source_bytes > self.limits.max_module_source_bytes {
            return Err(format!(
                "module source size {} exceeds limit {} in {}",
                source_bytes,
                self.limits.max_module_source_bytes,
                path.display()
            ));
        }
        let next_graph_source_bytes = self
            .graph_source_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| {
                format!(
                    "graph source size overflow while loading {}",
                    path.display()
                )
            })?;
        if next_graph_source_bytes > self.limits.max_graph_source_bytes {
            return Err(format!(
                "graph source size {} exceeds limit {} while loading {}",
                next_graph_source_bytes,
                self.limits.max_graph_source_bytes,
                path.display()
            ));
        }

        // Clone the Arc into a local so the resolver borrow is independent of the
        // mutable self borrows below (diagnostics / dependency_paths).
        let resolvers = Arc::clone(&self.resolvers);
        let mut resolver_context = ModuleResolverContext {
            resolver: resolvers.resolver(self.runtime),
            diagnostics: &mut self.graph.diagnostics,
            dependency_paths: &mut self.dependency_paths,
        };
        let mut prepared_source = prepare_module_source(&path, source)?;
        let parsed = match parse_module(
            &path,
            &prepared_source.source,
            &mut resolver_context,
            prepared_source.validate_semantics,
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                // A JS-like module shipping plain JSX (a package whose .js entry
                // ships untranspiled JSX) fails the mjs parse; retry via the JSX
                // transform so the bundler/minifier see JSX-free source. Anything
                // that still fails (Flow types, genuine syntax errors) returns the
                // original error and falls back safely.
                if module_can_retry_as_jsx(&path)
                    && let Ok(transformed) = transform_module_source_as(
                        &path,
                        &prepared_source.source,
                        SourceType::jsx(),
                    )
                {
                    prepared_source.source = transformed;
                    parse_module(&path, &prepared_source.source, &mut resolver_context, false)?
                } else {
                    return Err(error);
                }
            }
        };
        let id = ModuleId(self.graph.modules.len());
        let next_paths = parsed
            .imports
            .iter()
            .map(|edge| edge.resolved_path.clone())
            .chain(
                parsed
                    .reexports
                    .iter()
                    .map(|edge| edge.resolved_path.clone()),
            )
            .chain(
                parsed
                    .star_exports
                    .iter()
                    .map(|edge| edge.resolved_path.clone()),
            )
            .collect::<Vec<_>>();

        self.graph.path_to_id.insert(path.clone(), id);
        self.loading_paths.insert(path.clone());
        self.graph_source_bytes = next_graph_source_bytes;
        self.graph.modules.push(ModuleRecord {
            id,
            path: path.clone(),
            source: prepared_source.source,
            original_source_bytes: source_bytes,
            content_hash,
            content_len,
            content_mtime_millis,
            imports: parsed.imports,
            external_imports: parsed.external_imports,
            import_statement_spans: parsed.import_statement_spans,
            export_specifier_statement_spans: parsed.export_specifier_statement_spans,
            exports: parsed.exports,
            reexports: parsed.reexports,
            star_exports: parsed.star_exports,
            root_symbol_spans: parsed.root_symbol_spans,
            shorthand_spans: parsed.shorthand_spans,
            local_bindings: parsed.local_bindings,
            binding_dependencies: parsed.binding_dependencies,
            side_effect_references: parsed.side_effect_references,
            static_member_accesses: parsed.static_member_accesses,
        });

        for next_path in next_paths {
            self.load_module_from(&next_path, Some(&path))?;
        }

        self.loading_paths.remove(&path);
        Ok(id)
    }
}

#[derive(Debug, Default)]
struct ParsedModule {
    imports: Vec<ImportEdge>,
    external_imports: Vec<ExternalImportEdge>,
    import_statement_spans: Vec<(usize, usize)>,
    export_specifier_statement_spans: Vec<(usize, usize)>,
    exports: Vec<ExportRecord>,
    reexports: Vec<ReExportRecord>,
    star_exports: Vec<StarExportRecord>,
    local_bindings: Vec<String>,
    binding_dependencies: Vec<BindingDependencyRecord>,
    side_effect_references: Vec<String>,
    static_member_accesses: Vec<StaticMemberAccess>,
    root_symbol_spans: Vec<RootSymbolSpans>,
    shorthand_spans: Vec<(usize, usize)>,
}

struct ModuleResolverContext<'a> {
    resolver: &'a Resolver,
    diagnostics: &'a mut Vec<GraphDiagnostic>,
    dependency_paths: &'a mut HashSet<PathBuf>,
}

enum ModuleResolution {
    Internal(PathBuf),
    External,
    IgnoredExternal,
}

fn source_type_for_prepared_module() -> SourceType {
    // The graph and bundler operate on a prepared ESM-like source representation.
    // JSON modules are synthesized as ESM, and TS/JSX inputs are transformed before
    // graph parsing. Keep this as MJS even when the original file was .mts/.cts/.cjs.
    SourceType::mjs()
}

struct PreparedModuleSource {
    source: String,
    // Graph parsing only needs module-record structure for unchanged JS-like files
    // and transformed TS/JSX output. Full compiler syntax validation is deferred to
    // generated bundle/minifier boundaries, where invalid reachable output falls
    // back safely instead of spending a semantic pass on every dependency module.
    validate_semantics: bool,
}

fn prepare_module_source(path: &Path, source: String) -> Result<PreparedModuleSource, String> {
    if path_has_extension(path, "json") {
        return Ok(PreparedModuleSource {
            source: synthetic_json_module(path, &source)?,
            validate_semantics: true,
        });
    }

    if module_needs_transform(path) {
        return Ok(PreparedModuleSource {
            source: transform_module_source(path, &source)?,
            validate_semantics: false,
        });
    }

    Ok(PreparedModuleSource {
        source,
        validate_semantics: false,
    })
}

fn synthetic_json_module(path: &Path, source: &str) -> Result<String, String> {
    let json = serde_json::from_str::<Value>(source)
        .map_err(|error| format!("failed to parse JSON module {}: {error}", path.display()))?;
    let literal = serde_json::to_string(&json)
        .map_err(|error| format!("failed to encode JSON module {}: {error}", path.display()))?;
    let mut generated =
        format!("const __importLensJson = {literal};\nexport default __importLensJson;\n");

    if let Some(object) = json.as_object() {
        let mut keys = object.keys().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            if is_safe_js_identifier(key) {
                let quoted_key = serde_json::to_string(key).map_err(|error| {
                    format!("failed to encode JSON key in {}: {error}", path.display())
                })?;
                generated.push_str(&format!(
                    "export const {key} = __importLensJson[{quoted_key}];\n"
                ));
            }
        }
    }

    Ok(generated)
}

fn transform_module_source(path: &Path, source: &str) -> Result<String, String> {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    transform_module_source_as(path, source, source_type)
}

fn transform_module_source_as(
    path: &Path,
    source: &str,
    source_type: SourceType,
) -> Result<String, String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse module before transform {}; errors: {}",
            path.display(),
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut program = parsed.program;
    // `with_enum_eval(true)` is a hard requirement of `Transformer`, not a tuning knob:
    // lowering `enum E { A = 1 }` needs each member's evaluated value, and the
    // transformer panics rather than guess when the scoping was built without it.
    // Only this call site transforms TypeScript, so only it needs the flag.
    let semantic = SemanticBuilder::new().with_enum_eval(true).build(&program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "semantic validation failed before transform {}; errors: {}",
            path.display(),
            semantic
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let transform = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if transform.diagnostics.has_errors() {
        return Err(format!(
            "failed to transform module {}; errors: {}",
            path.display(),
            transform
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    Ok(Codegen::new()
        .with_options(CodegenOptions::default())
        .build(&program)
        .code)
}

fn module_can_retry_as_jsx(path: &Path) -> bool {
    !path_has_extension(path, "json") && !module_needs_transform(path)
}

fn module_needs_transform(path: &Path) -> bool {
    ["ts", "tsx", "mts", "cts", "jsx"]
        .iter()
        .any(|extension| path_has_extension(path, extension))
}

fn path_has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn is_safe_js_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|char| char == '_' || char == '$' || char.is_ascii_alphanumeric())
        && !matches!(
            value,
            "arguments"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "eval"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "null"
                | "new"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "return"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
                | "yield"
                | "await"
        )
}

fn parse_module(
    path: &Path,
    source: &str,
    resolver_context: &mut ModuleResolverContext<'_>,
    validate_semantics: bool,
) -> Result<ParsedModule, String> {
    let allocator = Allocator::default();
    let source_type = source_type_for_prepared_module();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse module {}; errors: {}",
            path.display(),
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    if validate_semantics {
        let semantic = SemanticBuilder::new_compiler().build(&parsed.program);
        if semantic.diagnostics.has_errors() {
            return Err(format!(
                "semantic validation failed for {}; errors: {}",
                path.display(),
                semantic
                    .diagnostics
                    .errors()
                    .map(|error| format!("{error:?}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }

    let edges_result = import_edges(path, &parsed.module_record, resolver_context)?;
    let analysis = root_scope_analysis(&parsed.program);
    Ok(ParsedModule {
        imports: edges_result.imports,
        external_imports: edges_result.external_imports,
        import_statement_spans: edges_result.import_statement_spans,
        export_specifier_statement_spans: export_specifier_statement_spans(&parsed.program),
        exports: export_records(&parsed.module_record),
        reexports: reexport_records(path, &parsed.module_record, resolver_context)?,
        star_exports: star_export_records(path, &parsed.module_record, resolver_context)?,
        local_bindings: local_bindings(&parsed.program),
        binding_dependencies: analysis.dependencies,
        side_effect_references: analysis.side_effect_references,
        static_member_accesses: analysis.static_member_accesses,
        root_symbol_spans: analysis.symbol_spans,
        shorthand_spans: shorthand_identifier_spans(&parsed.program)
            .into_iter()
            .collect(),
    })
}

struct ImportEdgesResult {
    imports: Vec<ImportEdge>,
    external_imports: Vec<ExternalImportEdge>,
    import_statement_spans: Vec<(usize, usize)>,
}

fn import_edges(
    path: &Path,
    module_record: &OxcModuleRecord<'_>,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<ImportEdgesResult, String> {
    let mut imports = Vec::new();
    let mut external_imports = Vec::new();
    let mut import_statement_spans = Vec::new();
    let mut binding_import_specifiers = HashSet::new();

    for requested_modules in module_record.requested_modules.values() {
        for request in requested_modules {
            if request.is_import && !request.is_type {
                let span = (
                    span_start(request.statement_span),
                    span_end(request.statement_span),
                );
                if !import_statement_spans.contains(&span) {
                    import_statement_spans.push(span);
                }
            }
        }
    }

    for entry in module_record
        .import_entries
        .iter()
        .filter(|entry| !entry.is_type)
    {
        let specifier = entry.module_request.name.as_str().to_owned();
        binding_import_specifiers.insert(specifier.clone());
        let imported_name = import_name(entry);
        let local_name = entry.local_name.name.as_str().to_owned();
        match resolve_module(path, &specifier, resolver_context)? {
            ModuleResolution::Internal(resolved_path) => {
                push_import_binding(
                    &mut imports,
                    specifier,
                    resolved_path,
                    imported_name,
                    local_name,
                );
            }
            ModuleResolution::External => {
                external_imports.push(ExternalImportEdge {
                    specifier,
                    imported_name,
                    local_name,
                });
            }
            ModuleResolution::IgnoredExternal => {}
        }
    }

    for (specifier, requested_modules) in &module_record.requested_modules {
        if binding_import_specifiers.contains(specifier.as_str()) {
            continue;
        }

        let is_side_effect_import = requested_modules
            .iter()
            .any(|request| request.is_import && !request.is_type);
        if !is_side_effect_import {
            continue;
        }

        match resolve_module(path, specifier.as_str(), resolver_context)? {
            ModuleResolution::Internal(resolved_path) => {
                imports.push(ImportEdge {
                    specifier: specifier.as_str().to_owned(),
                    resolved_path,
                    imported_names: Vec::new(),
                    imported_bindings: Vec::new(),
                });
            }
            ModuleResolution::External => {
                external_imports.push(ExternalImportEdge {
                    specifier: specifier.as_str().to_owned(),
                    imported_name: String::new(),
                    local_name: String::new(),
                });
            }
            ModuleResolution::IgnoredExternal => {}
        }
    }

    Ok(ImportEdgesResult {
        imports,
        external_imports,
        import_statement_spans,
    })
}

fn push_import_binding(
    imports: &mut Vec<ImportEdge>,
    specifier: String,
    resolved_path: PathBuf,
    imported_name: String,
    local_name: String,
) {
    if let Some(edge) = imports
        .iter_mut()
        .find(|edge| edge.specifier == specifier && edge.resolved_path == resolved_path)
    {
        if !edge.imported_names.contains(&imported_name) {
            edge.imported_names.push(imported_name.clone());
        }
        if !edge
            .imported_bindings
            .iter()
            .any(|binding| binding.local_name == local_name)
        {
            edge.imported_bindings.push(ImportedBinding {
                imported_name,
                local_name,
            });
        }
        return;
    }

    imports.push(ImportEdge {
        specifier,
        resolved_path,
        imported_names: vec![imported_name.clone()],
        imported_bindings: vec![ImportedBinding {
            imported_name,
            local_name,
        }],
    });
}

fn import_name(entry: &ImportEntry<'_>) -> String {
    match &entry.import_name {
        ImportImportName::Name(name) => name.name.as_str().to_owned(),
        ImportImportName::NamespaceObject => "*".to_owned(),
        ImportImportName::Default(_) => "default".to_owned(),
    }
}

fn export_records(module_record: &OxcModuleRecord<'_>) -> Vec<ExportRecord> {
    module_record
        .local_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| {
            let exported_name = export_export_name(&entry.export_name)?;
            let local_name =
                export_local_name(&entry.local_name).unwrap_or_else(|| exported_name.clone());
            Some(ExportRecord {
                exported_name,
                local_name,
                statement_start: span_start(entry.statement_span),
                statement_end: span_end(entry.statement_span),
            })
        })
        .collect()
}

fn reexport_records(
    path: &Path,
    module_record: &OxcModuleRecord<'_>,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<Vec<ReExportRecord>, String> {
    module_record
        .indirect_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| reexport_record(path, entry, resolver_context).transpose())
        .collect()
}

fn reexport_record(
    path: &Path,
    entry: &ExportEntry<'_>,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<Option<ReExportRecord>, String> {
    let Some(module_request) = &entry.module_request else {
        return Ok(None);
    };
    let resolved_path = match resolve_module(path, module_request.name.as_str(), resolver_context)?
    {
        ModuleResolution::Internal(resolved_path) => resolved_path,
        ModuleResolution::External | ModuleResolution::IgnoredExternal => return Ok(None),
    };
    let Some(exported_name) = export_export_name(&entry.export_name) else {
        return Ok(None);
    };
    let Some(imported_name) = export_import_name(&entry.import_name) else {
        return Ok(None);
    };

    Ok(Some(ReExportRecord {
        specifier: module_request.name.as_str().to_owned(),
        resolved_path,
        imported_name,
        exported_name,
        statement_start: span_start(entry.statement_span),
        statement_end: span_end(entry.statement_span),
    }))
}

fn star_export_records(
    path: &Path,
    module_record: &OxcModuleRecord<'_>,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<Vec<StarExportRecord>, String> {
    module_record
        .star_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| {
            let module_request = entry.module_request.as_ref()?;
            let resolved_path =
                match resolve_module(path, module_request.name.as_str(), resolver_context) {
                    Ok(ModuleResolution::Internal(resolved_path)) => resolved_path,
                    Ok(ModuleResolution::External | ModuleResolution::IgnoredExternal) => {
                        return None;
                    }
                    Err(error) => return Some(Err(error)),
                };
            Some(Ok(StarExportRecord {
                specifier: module_request.name.as_str().to_owned(),
                resolved_path,
                statement_start: span_start(entry.statement_span),
                statement_end: span_end(entry.statement_span),
            }))
        })
        .collect()
}

fn export_import_name(name: &ExportImportName<'_>) -> Option<String> {
    match name {
        ExportImportName::Name(name) => Some(name.name.as_str().to_owned()),
        ExportImportName::All => Some("*".to_owned()),
        ExportImportName::AllButDefault | ExportImportName::Null => None,
    }
}

fn export_export_name(name: &ExportExportName<'_>) -> Option<String> {
    match name {
        ExportExportName::Name(name) => Some(name.name.as_str().to_owned()),
        ExportExportName::Default(_) => Some("default".to_owned()),
        ExportExportName::Null => None,
    }
}

fn export_local_name(name: &ExportLocalName<'_>) -> Option<String> {
    match name {
        ExportLocalName::Name(name) | ExportLocalName::Default(name) => {
            Some(name.name.as_str().to_owned())
        }
        ExportLocalName::Null => None,
    }
}

fn export_specifier_statement_spans(program: &Program<'_>) -> Vec<(usize, usize)> {
    let mut spans = program
        .body
        .iter()
        .filter_map(|statement| match statement {
            Statement::ExportNamedDeclaration(export) if export.declaration.is_none() => {
                Some((span_start(export.span), span_end(export.span)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    spans.sort();
    spans.dedup();
    spans
}

fn local_bindings(program: &Program<'_>) -> Vec<String> {
    let mut bindings = Vec::new();
    for statement in &program.body {
        collect_statement_bindings(statement, &mut bindings);
    }
    bindings.sort();
    bindings.dedup();
    bindings
}

#[derive(Debug)]
struct StatementBindingRange {
    start: usize,
    end: usize,
    bindings: Vec<String>,
}

struct RootScopeAnalysis {
    dependencies: Vec<BindingDependencyRecord>,
    side_effect_references: Vec<String>,
    static_member_accesses: Vec<StaticMemberAccess>,
    symbol_spans: Vec<RootSymbolSpans>,
}

// One root-scope semantic pass yields both the binding-dependency edges and the
// declaration/reference spans the bundle rewriter needs, so the rewriter no
// longer re-parses each module per request. Always builds semantic (not gated on
// binding statements) so import bindings that have no declaration statement still
// get their rename spans recorded.
fn root_scope_analysis(program: &Program<'_>) -> RootScopeAnalysis {
    let semantic = SemanticBuilder::new().with_build_nodes(true).build(program);
    if semantic.diagnostics.has_errors() {
        return RootScopeAnalysis {
            dependencies: Vec::new(),
            side_effect_references: Vec::new(),
            static_member_accesses: Vec::new(),
            symbol_spans: Vec::new(),
        };
    }

    let semantic = semantic.semantic;
    let scoping = semantic.scoping();
    let mut references = Vec::new();
    let mut symbol_spans = Vec::new();
    for symbol_id in scoping.iter_bindings_in(scoping.root_scope_id()) {
        let name = scoping.symbol_name(symbol_id).to_owned();
        let decl_span = scoping.symbol_span(symbol_id);
        let mut symbol_references = Vec::new();
        for reference in semantic.symbol_references(symbol_id) {
            let span = semantic.reference_span(reference);
            references.push((span, name.clone()));
            symbol_references.push((span_start(span), span_end(span)));
        }
        symbol_spans.push(RootSymbolSpans {
            name,
            decl: (span_start(decl_span), span_end(decl_span)),
            references: symbol_references,
        });
    }

    RootScopeAnalysis {
        dependencies: binding_dependencies_from(&statement_binding_ranges(program), &references),
        side_effect_references: side_effect_reference_names(
            &side_effect_statement_ranges(program),
            &references,
        ),
        static_member_accesses: static_member_accesses(program),
        symbol_spans,
    }
}

fn static_member_accesses(program: &Program<'_>) -> Vec<StaticMemberAccess> {
    let mut collector = StaticMemberAccessCollector::default();
    collector.visit_program(program);
    collector.accesses
}

#[derive(Default)]
struct StaticMemberAccessCollector {
    accesses: Vec<StaticMemberAccess>,
}

impl<'a> Visit<'a> for StaticMemberAccessCollector {
    fn visit_static_member_expression(&mut self, expression: &StaticMemberExpression<'a>) {
        // `ns?.alpha` is deliberately excluded: rewriting it to a bare binding
        // would drop the nullish guard. A module namespace is never nullish, so
        // the conservative escape path is the honest answer rather than a
        // silent semantic change.
        if !expression.optional
            && let Expression::Identifier(identifier) = &expression.object
        {
            self.accesses.push(StaticMemberAccess {
                object: span_bounds(identifier.span),
                span: span_bounds(expression.span),
                property: expression.property.name.as_str().to_owned(),
            });
        }

        walk::walk_static_member_expression(self, expression);
    }
}

/// Spans of top-level statements the rewriter keeps verbatim and that bind
/// nothing -- `setup(dep);`, `globalThis.x = dep;`. Import and export
/// declarations are excluded because the rewriter rewrites or removes them, and
/// statements that do declare a binding are already covered by
/// `binding_dependencies_from`. Source-ordered and non-overlapping.
fn side_effect_statement_ranges(program: &Program<'_>) -> Vec<(usize, usize)> {
    program
        .body
        .iter()
        .filter_map(|statement| {
            if matches!(
                statement,
                Statement::ImportDeclaration(_)
                    | Statement::ExportNamedDeclaration(_)
                    | Statement::ExportAllDeclaration(_)
                    | Statement::ExportDefaultDeclaration(_)
            ) {
                return None;
            }

            let mut bindings = Vec::new();
            collect_statement_bindings(statement, &mut bindings);
            if !bindings.is_empty() {
                return None;
            }

            let span = statement.span();
            Some((span_start(span), span_end(span)))
        })
        .collect()
}

fn side_effect_reference_names(
    statement_ranges: &[(usize, usize)],
    references: &[(Span, String)],
) -> Vec<String> {
    if statement_ranges.is_empty() {
        return Vec::new();
    }

    let mut names = references
        .iter()
        .filter(|(span, _)| {
            // Ranges are source-ordered and non-overlapping, so the only range
            // that can contain this reference is the last one starting at or
            // before it.
            let index = statement_ranges.partition_point(|(start, _)| *start <= span_start(*span));
            index > 0 && span_end(*span) <= statement_ranges[index - 1].1
        })
        .map(|(_, name)| name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn binding_dependencies_from(
    statement_ranges: &[StatementBindingRange],
    references: &[(Span, String)],
) -> Vec<BindingDependencyRecord> {
    if statement_ranges.is_empty() {
        return Vec::new();
    }

    // Sort references by start once and, for each (source-ordered, non-
    // overlapping) statement range, binary-search to the first reference at or
    // after its start and walk forward until past its end. This replaces the
    // O(statements x references) rescan with O((statements + references) log n),
    // which matters for large single-file modules (up to the 20 MiB cap).
    let mut sorted_refs: Vec<&(Span, String)> = references.iter().collect();
    sorted_refs.sort_by_key(|(span, _)| span_start(*span));

    let mut dependencies = Vec::new();
    for range in statement_ranges {
        let first = sorted_refs.partition_point(|(span, _)| span_start(*span) < range.start);
        for (span, referenced_name) in &sorted_refs[first..] {
            if span_start(*span) > range.end {
                break;
            }
            if span_end(*span) > range.end {
                continue;
            }

            for binding_name in &range.bindings {
                if binding_name == referenced_name {
                    continue;
                }

                dependencies.push(BindingDependencyRecord {
                    binding_name: binding_name.clone(),
                    referenced_name: referenced_name.clone(),
                });
            }
        }
    }

    dependencies.sort_by(|left, right| {
        left.binding_name
            .cmp(&right.binding_name)
            .then_with(|| left.referenced_name.cmp(&right.referenced_name))
    });
    dependencies.dedup_by(|left, right| {
        left.binding_name == right.binding_name && left.referenced_name == right.referenced_name
    });
    dependencies
}

fn statement_binding_ranges(program: &Program<'_>) -> Vec<StatementBindingRange> {
    program
        .body
        .iter()
        .filter_map(|statement| {
            let mut bindings = Vec::new();
            collect_statement_bindings(statement, &mut bindings);
            bindings.sort();
            bindings.dedup();
            let span = statement.span();
            (!bindings.is_empty()).then_some(StatementBindingRange {
                start: span_start(span),
                end: span_end(span),
                bindings,
            })
        })
        .collect()
}

fn collect_statement_bindings(statement: &Statement<'_>, bindings: &mut Vec<String>) {
    match statement {
        Statement::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                collect_binding_pattern(&declarator.id, bindings);
            }
        }
        Statement::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(declaration) = &export.declaration {
                collect_declaration_bindings(declaration, bindings);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            collect_export_default_bindings(&export.declaration, bindings);
        }
        _ => {}
    }
}

fn collect_declaration_bindings(declaration: &Declaration<'_>, bindings: &mut Vec<String>) {
    match declaration {
        Declaration::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                collect_binding_pattern(&declarator.id, bindings);
            }
        }
        Declaration::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        _ => {}
    }
}

fn collect_export_default_bindings(
    declaration: &ExportDefaultDeclarationKind<'_>,
    bindings: &mut Vec<String>,
) {
    match declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.push(id.name.as_str().to_owned());
            } else {
                bindings.push("default".to_owned());
            }
        }
        ExportDefaultDeclarationKind::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.push(id.name.as_str().to_owned());
            } else {
                bindings.push("default".to_owned());
            }
        }
        _ => bindings.push("default".to_owned()),
    }
}

fn collect_binding_pattern(pattern: &BindingPattern<'_>, bindings: &mut Vec<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            bindings.push(identifier.name.as_str().to_owned());
        }
        BindingPattern::AssignmentPattern(assignment) => {
            collect_binding_pattern(&assignment.left, bindings);
        }
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                collect_binding_pattern(&property.value, bindings);
            }
            if let Some(rest) = &object.rest {
                collect_binding_pattern(&rest.argument, bindings);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                collect_binding_pattern(element, bindings);
            }
            if let Some(rest) = &array.rest {
                collect_binding_pattern(&rest.argument, bindings);
            }
        }
    }
}

fn resolve_module(
    from_path: &Path,
    specifier: &str,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<ModuleResolution, String> {
    if specifier_has_query_or_fragment(specifier) {
        push_resolution_diagnostic(
            resolver_context,
            from_path,
            specifier,
            format!("import '{specifier}' contains an unsupported query or fragment"),
        );
        return Ok(ModuleResolution::IgnoredExternal);
    }

    if let Some(kind) = asset_import_kind(specifier) {
        push_asset_diagnostic(resolver_context, from_path, specifier, kind);
        return Ok(ModuleResolution::IgnoredExternal);
    }

    if is_node_builtin_specifier(specifier) {
        push_resolution_diagnostic(
            resolver_context,
            from_path,
            specifier,
            format!("module '{specifier}' is a Node builtin and was kept external"),
        );
        return Ok(ModuleResolution::External);
    }

    let from_dir = from_path.parent().ok_or_else(|| {
        format!(
            "module path has no parent directory: {}",
            from_path.display()
        )
    })?;

    let resolved = match resolve_module_path(resolver_context.resolver, from_dir, specifier) {
        Ok(resolved) => resolved,
        Err(error) => {
            if specifier.starts_with('.') {
                return Err(format!(
                    "failed to resolve relative module '{specifier}' from {}; {error}",
                    from_path.display()
                ));
            }

            push_resolution_diagnostic(
                resolver_context,
                from_path,
                specifier,
                format!("failed to resolve external peer '{specifier}': {error}"),
            );
            return Ok(ModuleResolution::External);
        }
    };

    resolver_context
        .dependency_paths
        .insert(resolved.path.clone());
    Ok(ModuleResolution::Internal(resolved.path))
}

fn specifier_has_query_or_fragment(specifier: &str) -> bool {
    specifier.contains('?') || specifier.contains('#')
}

fn asset_import_kind(specifier: &str) -> Option<&'static str> {
    let extension = Path::new(specifier)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())?
        .to_ascii_lowercase();

    if is_javascript_module_extension(&extension) {
        return None;
    }

    match extension.as_str() {
        "css" | "scss" | "sass" | "less" | "styl" => Some("style"),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" => Some("image"),
        "ttf" | "otf" | "woff" | "woff2" | "eot" => Some("font"),
        _ if specifier.starts_with('.') || specifier.starts_with('/') => Some("asset"),
        _ => None,
    }
}

fn is_javascript_module_extension(extension: &str) -> bool {
    matches!(
        extension,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" | "json"
    )
}

fn push_asset_diagnostic(
    resolver_context: &mut ModuleResolverContext<'_>,
    from_path: &Path,
    specifier: &str,
    kind: &str,
) {
    resolver_context.diagnostics.push(GraphDiagnostic {
        stage: "asset".to_owned(),
        message: format!("non-JavaScript {kind} import kept external: {specifier}"),
        details: vec![
            format!("from_path: {}", from_path.display()),
            format!("specifier: {specifier}"),
            format!("asset_kind: {kind}"),
        ],
    });
}

fn push_resolution_diagnostic(
    resolver_context: &mut ModuleResolverContext<'_>,
    from_path: &Path,
    specifier: &str,
    message: String,
) {
    resolver_context.diagnostics.push(GraphDiagnostic {
        stage: "module_resolution".to_owned(),
        message,
        details: vec![
            format!("from_path: {}", from_path.display()),
            format!("specifier: {specifier}"),
        ],
    });
}

/// Sorted for binary search. Shared with the candidate engine, which hands
/// Rolldown the same boundary as an exact-match external specifier list.
pub const NODE_BUILTIN_MODULES: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "inspector/promises",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "timers",
    "timers/promises",
    "tls",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

pub fn is_node_builtin_specifier(specifier: &str) -> bool {
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    NODE_BUILTIN_MODULES.binary_search(&bare).is_ok()
}

#[cfg(test)]
mod node_builtin_tests {
    use super::NODE_BUILTIN_MODULES;

    /// `is_node_builtin_specifier` binary-searches the list, so an entry
    /// added out of order would silently stop matching.
    #[test]
    fn node_builtin_modules_stay_strictly_sorted() {
        assert!(
            NODE_BUILTIN_MODULES
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "NODE_BUILTIN_MODULES must stay strictly sorted for binary search"
        );
    }
}

/// Every name `module_id` exports, transitively through re-exports and star
/// exports. `include_default` is false for star-export recursion because
/// `export *` never forwards `default`. Sorted and deduplicated so callers get
/// a stable order.
pub fn module_exported_names(
    graph: &ModuleGraph,
    module_id: ModuleId,
    include_default: bool,
) -> Vec<String> {
    let mut names = Vec::new();
    collect_module_exports(
        graph,
        module_id,
        include_default,
        &mut HashSet::new(),
        &mut names,
    );
    names.sort();
    names.dedup();
    names
}

fn collect_module_exports(
    graph: &ModuleGraph,
    module_id: ModuleId,
    include_default: bool,
    visited: &mut HashSet<ModuleId>,
    exports: &mut Vec<String>,
) {
    if !visited.insert(module_id) {
        return;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return;
    };

    exports.extend(
        module
            .exports
            .iter()
            .filter(|export| include_default || export.exported_name != "default")
            .map(|export| export.exported_name.clone()),
    );
    exports.extend(
        module
            .reexports
            .iter()
            .filter(|reexport| include_default || reexport.exported_name != "default")
            .map(|reexport| reexport.exported_name.clone()),
    );

    for star_export in &module.star_exports {
        if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path) {
            collect_module_exports(graph, target_id, false, visited, exports);
        }
    }
}

pub fn module_provides_export(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> bool {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return false;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return false;
    };

    if module
        .exports
        .iter()
        .any(|export| export.exported_name == exported_name)
    {
        return true;
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        if reexport.imported_name == "*" {
            return true;
        }

        if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path)
            && module_provides_export(graph, target_id, &reexport.imported_name, visited)
        {
            return true;
        }
    }

    for star_export in &module.star_exports {
        if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path)
            && module_provides_export(graph, target_id, exported_name, visited)
        {
            return true;
        }
    }

    false
}

fn span_start(span: Span) -> usize {
    span.start as usize
}

fn span_end(span: Span) -> usize {
    span.end as usize
}

fn span_bounds(span: Span) -> (usize, usize) {
    (span_start(span), span_end(span))
}

pub(crate) fn shorthand_identifier_spans(program: &Program<'_>) -> HashSet<(usize, usize)> {
    let mut collector = ShorthandIdentifierCollector::default();
    collector.visit_program(program);
    collector.spans
}

#[derive(Default)]
struct ShorthandIdentifierCollector {
    spans: HashSet<(usize, usize)>,
}

impl<'a> Visit<'a> for ShorthandIdentifierCollector {
    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if property.shorthand
            && let Expression::Identifier(identifier) = &property.value
        {
            self.spans.insert(span_bounds(identifier.span));
        }

        walk::walk_object_property(self, property);
    }

    fn visit_binding_property(&mut self, property: &BindingProperty<'a>) {
        if property.shorthand {
            collect_binding_pattern_spans(&property.value, &mut self.spans);
        }

        walk::walk_binding_property(self, property);
    }

    fn visit_assignment_target_property_identifier(
        &mut self,
        property: &AssignmentTargetPropertyIdentifier<'a>,
    ) {
        self.spans.insert(span_bounds(property.binding.span));
        walk::walk_assignment_target_property_identifier(self, property);
    }
}

fn collect_binding_pattern_spans(
    pattern: &BindingPattern<'_>,
    spans: &mut HashSet<(usize, usize)>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            spans.insert(span_bounds(identifier.span));
        }
        BindingPattern::AssignmentPattern(pattern) => {
            collect_binding_pattern_spans(&pattern.left, spans);
        }
        BindingPattern::ObjectPattern(pattern) => {
            for property in &pattern.properties {
                collect_binding_pattern_spans(&property.value, spans);
            }
            if let Some(rest) = &pattern.rest {
                collect_binding_pattern_spans(&rest.argument, spans);
            }
        }
        BindingPattern::ArrayPattern(pattern) => {
            for element in pattern.elements.iter().flatten() {
                collect_binding_pattern_spans(element, spans);
            }
            if let Some(rest) = &pattern.rest {
                collect_binding_pattern_spans(&rest.argument, spans);
            }
        }
    }
}

#[cfg(test)]
mod fingerprint_canonicalize_tests {
    use super::*;
    use crate::cache::key::{FileFingerprint, file_fingerprint_reading_hash};
    use crate::ipc::protocol::ImportRuntime;
    use std::fs;
    use std::path::PathBuf;

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "il-c8-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Reference implementation of the PRE-CHANGE fingerprint algorithm: it
    /// `fs::canonicalize`s EVERY input path to find its module (the redundant
    /// per-module syscall Task C8 removes) and rebuilds each module fingerprint
    /// through the original `normalize_identity_path` (canonicalize-then-slash),
    /// falling back to a stat-only fingerprint for non-module paths. Byte-identical
    /// output to this oracle is the proof that the optimization did not change the
    /// produced fingerprints (same paths + hash + len + mtime + sort + dedup).
    fn reference_pre_change(paths: &[PathBuf], graph: &ModuleGraph) -> Vec<FileFingerprint> {
        let mut fingerprints: Vec<FileFingerprint> = paths
            .iter()
            .filter_map(|path| {
                let via_module = std::fs::canonicalize(path)
                    .ok()
                    .and_then(|canonical| graph.module_id_by_path(&canonical))
                    .and_then(|id| graph.module_by_id(id))
                    .map(|module| FileFingerprint {
                        // The original `normalize_identity_path(&module.path)`.
                        path: std::fs::canonicalize(&module.path)
                            .unwrap_or_else(|_| module.path.clone())
                            .to_string_lossy()
                            .replace('\\', "/"),
                        len: module.content_len,
                        modified_millis: module.content_mtime_millis,
                        content_hash: Some(module.content_hash),
                    });
                via_module.or_else(|| file_fingerprint_reading_hash(path))
            })
            .collect();
        fingerprints.sort_by(|a, b| a.path.cmp(&b.path));
        fingerprints.dedup_by(|a, b| a.path == b.path);
        fingerprints
    }

    #[test]
    fn fingerprints_reuse_graph_canonical_paths_without_per_module_canonicalize() {
        let dir = unique_dir("reuse");
        // Canonicalize the root so every child path is already the graph's canonical
        // module-key spelling (exactly how the resolver hands paths to the real
        // callers), which is what lets the raw-path lookup hit without a syscall.
        let root = fs::canonicalize(&dir).expect("canonical root");
        let entry = root.join("entry.mjs");
        let deps: Vec<PathBuf> = (0..5).map(|i| root.join(format!("dep{i}.mjs"))).collect();
        for (i, dep) in deps.iter().enumerate() {
            fs::write(dep, format!("export const v{i} = {i};\n")).expect("write dep");
        }
        let mut entry_src = String::new();
        for i in 0..deps.len() {
            entry_src.push_str(&format!("import {{ v{i} }} from './dep{i}.mjs';\n"));
        }
        entry_src.push_str("export const total = 0;\n");
        fs::write(&entry, entry_src).expect("write entry");
        let package_json = root.join("package.json");
        fs::write(&package_json, "{\"name\":\"c8\",\"version\":\"1.0.0\"}\n").expect("pkg");

        let graph =
            build_module_graph_with_runtime(&entry, ImportRuntime::Component).expect("build graph");
        assert_eq!(
            graph.modules.len(),
            deps.len() + 1,
            "entry + every dep must be loaded as a module"
        );

        // Mirror `service::dependency_fingerprints`' hot-path input set: the
        // non-module manifest, the (canonical) entry, every graph module, and the
        // resolver's dependency paths. All but `package.json` are ALREADY the graph's
        // canonical keys.
        let mut paths = vec![package_json.clone(), entry.clone()];
        paths.extend(graph.modules.iter().map(|module| module.path.clone()));
        paths.extend(graph.dependency_paths.iter().cloned());

        // CORRECTNESS (the important half): byte-identical to the pre-change output.
        let expected = reference_pre_change(&paths, &graph);
        let actual = fingerprints_with_content_hashes(paths.clone(), &graph);
        assert_eq!(
            actual, expected,
            "optimized fingerprints must be byte-identical to the pre-change algorithm"
        );
        // Non-vacuous: every loaded module carries its read-time content hash, and
        // the non-module manifest is now read+hashed too (RB-2), so ALL fingerprints
        // are content-hashed.
        assert!(
            actual
                .iter()
                .all(|fingerprint| fingerprint.content_hash.is_some()),
            "every fingerprint (modules + read-hashed manifest) must carry a content hash: {actual:?}"
        );
        assert_eq!(
            actual.len(),
            graph.modules.len() + 1,
            "the fingerprint set is every module plus the read-hashed manifest"
        );

        // PERF: building those fingerprints canonicalizes only the non-module path
        // (package.json), NOT once per module. Before the fix this was O(N paths).
        canonicalize_probe::arm();
        let _ = fingerprints_with_content_hashes(paths, &graph);
        let canonicalizes = canonicalize_probe::hits();
        canonicalize_probe::disarm();
        assert_eq!(
            canonicalizes,
            1,
            "only the non-module package.json should hit fs::canonicalize; the \
             entry + {} module/dep paths must reuse the graph's canonical keys",
            graph.modules.len()
        );

        fs::remove_dir_all(dir).ok();
    }
}
