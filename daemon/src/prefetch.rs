use crate::{
    cache::key::{CacheIdentity, decode_cache_identity},
    engine::{EngineBudget, scheduling::drain_ordered},
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::{
        analyze::AnalysisContext,
        resolver::{ResolvedPackage, resolve_package_entry, resolved_from_cache_identity},
    },
    service::ImportLensService,
};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::module_record::{ExportEntry, ExportExportName};
use rayon::ThreadPoolBuilder;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static PREWARM_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
const RECENT_PREWARM_LIMIT: usize = 20;
const DEFAULT_EXPORT_MEMO_LIMIT: usize = 512;

// entry_path -> (entry stat token, exposes default). Prewarm reruns on every
// package.json event and export enumeration is an uncached engine build, so a
// default-less dependency would otherwise cost one build per event forever.
static DEFAULT_EXPORT_MEMO: OnceLock<Mutex<HashMap<PathBuf, (u64, bool)>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct PrewarmJob {
    request: ImportRequest,
    resolved: ResolvedPackage,
}

#[derive(Debug, Default)]
pub struct CancellationToken {
    generation: AtomicU64,
}

impl CancellationToken {
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        let _ = self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }
}

#[derive(Debug, Default)]
pub struct Prefetcher {
    cancellation: Arc<CancellationToken>,
}

impl Prefetcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancellation(&self) -> &Arc<CancellationToken> {
        &self.cancellation
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn prewarm_package_json(
        &self,
        service: Arc<ImportLensService>,
        package_json_path: PathBuf,
        active_document_path: PathBuf,
    ) {
        let cancellation = Arc::clone(&self.cancellation);
        let generation = cancellation.next_generation();

        dispatch_prewarm(move || {
            run_prewarm_job(
                service,
                package_json_path,
                active_document_path,
                cancellation,
                generation,
            );
        });
    }

    pub fn prewarm_recent_cache_entries(
        &self,
        service: Arc<ImportLensService>,
        workspace_root: PathBuf,
    ) {
        let cancellation = Arc::clone(&self.cancellation);
        let generation = cancellation.next_generation();

        dispatch_prewarm(move || {
            run_recent_prewarm_job(service, workspace_root, cancellation, generation);
        });
    }
}

/// Dispatch the outer prewarm coordination (dependency enumeration + fan-out)
/// onto the bounded `PREWARM_POOL` instead of an unbounded per-call OS thread.
/// The heavy per-import work already runs on this pool via `pool.install`; this
/// bounds the OUTER dispatch too and, crucially, LOGS a pool-build failure at
/// debug rather than silently swallowing it as the old raw
/// `thread::Builder…spawn` (`let _ = …`) did. Cancellation is still checked inside
/// the job, so a superseded dispatch bails via the generation guard.
fn dispatch_prewarm(job: impl FnOnce() + Send + 'static) {
    match prewarm_pool() {
        Ok(pool) => pool.spawn(job),
        Err(error) => {
            crate::logging::log_debug("prefetch", format!("prewarm dispatch skipped: {error}"));
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn package_json_dependency_names(contents: &str) -> Result<Vec<String>, String> {
    let json = serde_json::from_str::<Value>(contents)
        .map_err(|error| format!("failed to parse package.json: {error}"))?;
    let mut names = Vec::new();

    for field in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        let Some(dependencies) = json.get(field).and_then(Value::as_object) else {
            continue;
        };

        for (name, version) in dependencies {
            if version.is_string() {
                names.push(name.to_owned());
            }
        }
    }

    names.sort();
    names.dedup();
    Ok(names)
}

pub fn package_json_prewarm_requests(
    package_json_path: &Path,
    active_document_path: &Path,
) -> Result<Vec<ImportRequest>, String> {
    Ok(
        package_json_prewarm_jobs(package_json_path, active_document_path, &|| true)?
            .into_iter()
            .map(|job| job.request)
            .collect(),
    )
}

fn package_json_prewarm_jobs(
    package_json_path: &Path,
    active_document_path: &Path,
    should_continue: &dyn Fn() -> bool,
) -> Result<Vec<PrewarmJob>, String> {
    let contents = fs::read_to_string(package_json_path).map_err(|error| {
        format!(
            "failed to read package.json {}: {error}",
            package_json_path.display()
        )
    })?;
    let mut requests = Vec::new();

    for package_name in package_json_dependency_names(&contents)? {
        if !should_continue() {
            return Ok(Vec::new());
        }
        let Some(resolved) = installed_package(active_document_path, &package_name) else {
            continue;
        };
        let Some(version) = resolved
            .package_json
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };

        if exposes_default_export(&resolved) {
            requests.push(PrewarmJob {
                request: prewarm_request(&package_name, &version, ImportKind::Default),
                resolved: resolved.clone(),
            });
        }
        requests.push(PrewarmJob {
            request: prewarm_request(&package_name, &version, ImportKind::Namespace),
            resolved,
        });
    }

    Ok(requests)
}

// Avoid a guaranteed missing-export build: a package with no default export must not
// get a Default prewarm job. The question is only ever about the entry file's own
// export statements, so it is answered by parsing that one file — it used to be
// answered with a full `enumerate_exports_sync` engine build of the entire package
// graph, once per dependency, serially, before any real prewarm work could start.
//
// Still memoized per entry stat token: prewarm reruns on every package.json event.
fn exposes_default_export(resolved: &ResolvedPackage) -> bool {
    let token = entry_stat_token(&resolved.entry_path);
    let memo = DEFAULT_EXPORT_MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(memo) = memo.lock()
        && let Some((cached_token, has_default)) = memo.get(&resolved.entry_path)
        && *cached_token == token
    {
        return *has_default;
    }

    let has_default = entry_exposes_default_export(&resolved.entry_path);

    if let Ok(mut memo) = memo.lock() {
        // Crude but sufficient bound: prewarm sweeps one dependency tree, so a
        // rare full reset just re-probes on the next sweep.
        if memo.len() >= DEFAULT_EXPORT_MEMO_LIMIT && !memo.contains_key(&resolved.entry_path) {
            memo.clear();
        }
        memo.insert(resolved.entry_path.clone(), (token, has_default));
    }
    has_default
}

/// Whether a package entry exposes a default export, without building anything.
pub fn entry_exposes_default_export(entry_path: &Path) -> bool {
    let Ok(source) = fs::read_to_string(entry_path) else {
        return true; // unreadable: keep the job rather than silently drop it
    };
    let source_type = SourceType::from_path(entry_path).unwrap_or_else(|_| SourceType::mjs());
    source_exposes_default(&source, source_type)
}

/// Whether an entry file exposes a default export.
///
/// Every answer defaults to `true` when unsure: a spurious Default job costs one
/// wasted prewarm, while a wrongly-dropped one costs a cache miss on the exact
/// import the user is about to type.
fn source_exposes_default(source: &str, source_type: SourceType) -> bool {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return true;
    }

    let record = &parsed.module_record;
    // No ESM syntax at all: CommonJS, where interop synthesizes a default from
    // `module.exports`. Keep the job.
    if !record.has_module_syntax {
        return true;
    }

    let exports_default = |entry: &ExportEntry| match &entry.export_name {
        ExportExportName::Default(_) => true,
        ExportExportName::Name(name) => name.name == "default",
        ExportExportName::Null => false,
    };

    // `export default …` and `export { x as default }` land in local entries;
    // `export { default } from './x'` in indirect ones. A bare `export * from` does
    // NOT re-export the default (the spec excludes it), but `export * as default
    // from` does — and that is a star entry carrying the name.
    record.local_export_entries.iter().any(exports_default)
        || record.indirect_export_entries.iter().any(exports_default)
        || record.star_export_entries.iter().any(exports_default)
}

/// Cheap edit-sensitivity token (length + mtime) so an entry rewrite re-probes.
fn entry_stat_token(path: &Path) -> u64 {
    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    metadata.len() ^ modified.rotate_left(32)
}

fn run_prewarm_job(
    service: Arc<ImportLensService>,
    package_json_path: PathBuf,
    active_document_path: PathBuf,
    cancellation: Arc<CancellationToken>,
    generation: u64,
) {
    if !cancellation.is_current(generation) {
        return;
    }

    let Ok(jobs) = package_json_prewarm_jobs(&package_json_path, &active_document_path, &|| {
        cancellation.is_current(generation)
    }) else {
        return;
    };

    if jobs.is_empty() || !cancellation.is_current(generation) {
        return;
    }

    let context = AnalysisContext {
        workspace_root: package_json_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| active_document_path.clone()),
        active_document_path,
        // Prewarm answers no request: nobody is waiting on a deadline, and abandoning a build
        // here would only mean the user pays for it later, in the request that does wait. Each
        // build is still capped by `BUILD_TIMEOUT`, so a parked one cannot hold a permit.
        engine_budget: EngineBudget::background(),
    };

    drain_ordered(&jobs, |_, job| {
        if cancellation.is_current(generation) {
            service.prewarm_resolved_import(&context, &job.request, job.resolved.clone(), || {
                cancellation.is_current(generation)
            });
        }
    });
}

fn run_recent_prewarm_job(
    service: Arc<ImportLensService>,
    workspace_root: PathBuf,
    cancellation: Arc<CancellationToken>,
    generation: u64,
) {
    if !cancellation.is_current(generation) {
        return;
    }

    let jobs = service
        .recent_cache_keys(&workspace_root, RECENT_PREWARM_LIMIT)
        .into_iter()
        .filter_map(|key| {
            let identity = decode_cache_identity(&key)?;
            let resolved = resolved_from_cache_identity(&identity)?;
            let request = import_request_from_identity(identity);
            Some(PrewarmJob { request, resolved })
        })
        .collect::<Vec<_>>();

    if jobs.is_empty() || !cancellation.is_current(generation) {
        return;
    }

    let active_document_path = jobs
        .first()
        .and_then(|job| job.resolved.entry_path.parent().map(PathBuf::from))
        .unwrap_or_else(|| workspace_root.join("package.json"));
    let context = AnalysisContext {
        workspace_root,
        active_document_path,
        engine_budget: EngineBudget::background(),
    };
    drain_ordered(&jobs, |_, job| {
        if cancellation.is_current(generation) {
            service.prewarm_resolved_import(&context, &job.request, job.resolved.clone(), || {
                cancellation.is_current(generation)
            });
        }
    });
}

fn installed_package(active_document_path: &Path, package_name: &str) -> Option<ResolvedPackage> {
    let request = prewarm_request(package_name, "", ImportKind::Namespace);
    resolve_package_entry(active_document_path, &request).ok()
}

pub fn cached_import_request_from_key(key: &str) -> Option<ImportRequest> {
    decode_cache_identity(key).map(import_request_from_identity)
}

fn import_request_from_identity(identity: CacheIdentity) -> ImportRequest {
    ImportRequest {
        specifier: identity.specifier,
        package_name: identity.package_name,
        version: identity.package_version,
        named: identity.named_exports,
        import_kind: identity.import_kind,
        runtime: identity.runtime,
    }
}

fn prewarm_request(package_name: &str, version: &str, import_kind: ImportKind) -> ImportRequest {
    ImportRequest {
        specifier: package_name.to_owned(),
        package_name: package_name.to_owned(),
        version: version.to_owned(),
        named: Vec::new(),
        import_kind,
        runtime: ImportRuntime::Component,
    }
}

fn prewarm_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|value| (value.get() / 2).max(1))
        .unwrap_or(1)
}

pub fn prewarm_pool() -> Result<&'static rayon::ThreadPool, String> {
    if PREWARM_POOL.get().is_none() {
        let pool = ThreadPoolBuilder::new()
            .num_threads(prewarm_thread_count())
            .build()
            .map_err(|error| format!("failed to build prewarm thread pool: {error}"))?;
        let _ = PREWARM_POOL.set(pool);
    }

    PREWARM_POOL
        .get()
        .ok_or_else(|| "failed to initialize prewarm thread pool".to_owned())
}

#[cfg(test)]
mod tests {
    use super::dispatch_prewarm;
    use std::sync::mpsc;
    use std::time::Duration;

    // F3-A: the outer prewarm dispatch runs the coordination job on the bounded
    // `PREWARM_POOL` instead of an unbounded per-call OS thread. Proven by observing
    // the job actually execute on the pool — the raw `thread::Builder…spawn` with a
    // swallowed spawn error is gone.
    #[test]
    fn dispatch_prewarm_runs_job_on_bounded_pool() {
        let (tx, rx) = mpsc::channel();
        dispatch_prewarm(move || {
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "dispatch_prewarm should run the job on the bounded prewarm pool"
        );
    }
}

#[cfg(test)]
mod default_export_tests {
    use super::source_exposes_default;
    use oxc_span::SourceType;

    fn exposes(source: &str) -> bool {
        source_exposes_default(source, SourceType::mjs())
    }

    /// Answering this with a parse of the entry file replaced a full engine build of
    /// the whole package graph, once per dependency. The predicate decides whether a
    /// Default prewarm job exists at all, so every arm is pinned: a false negative
    /// silently costs the user a cache miss on the import they are about to type.
    #[test]
    fn detects_every_shape_of_default_export() {
        assert!(exposes("export default function go() {}\n"));
        assert!(exposes("const go = 1;\nexport { go as default };\n"));
        assert!(exposes("export { default } from './inner.js';\n"));
        assert!(exposes("export { inner as default } from './inner.js';\n"));
        assert!(exposes("export * as default from './inner.js';\n"));
    }

    #[test]
    fn rejects_a_package_with_only_named_exports() {
        assert!(!exposes(
            "export const alpha = 1;\nexport function beta() {}\n"
        ));
        assert!(!exposes("const x = 1;\nexport { x };\n"));
        assert!(!exposes("export { alpha, beta } from './inner.js';\n"));
    }

    /// A bare star re-export does NOT forward the default — the spec excludes it — so
    /// claiming otherwise would queue a build guaranteed to fail on a missing export.
    #[test]
    fn a_bare_star_reexport_does_not_forward_the_default() {
        assert!(!exposes("export * from './inner.js';\n"));
    }

    /// Unsure means yes: a spurious Default job wastes one prewarm; a wrongly dropped
    /// one costs a miss on the exact import the user reaches for.
    #[test]
    fn stays_conservative_when_it_cannot_know() {
        // CommonJS: interop synthesizes a default from `module.exports`.
        assert!(exposes("module.exports = function go() {};\n"));
        // No exports at all.
        assert!(exposes("const unused = 1;\n"));
        // Unparseable.
        assert!(exposes("export default function ( {{{ \n"));
    }
}
