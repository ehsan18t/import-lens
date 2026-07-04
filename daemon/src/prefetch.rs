use crate::{
    cache::key::{CacheIdentityV3, decode_cache_identity},
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::{
        analyze::AnalysisContext,
        graph::{cached_module_graph_with_runtime, module_provides_export},
        resolver::{ResolvedPackage, resolve_package_entry, resolved_from_cache_identity},
    },
    service::ImportLensService,
};
use rayon::{ThreadPoolBuilder, prelude::*};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

static PREWARM_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
const RECENT_PREWARM_LIMIT: usize = 20;

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

        let _ = thread::Builder::new()
            .name("import-lens-prewarm".to_owned())
            .spawn(move || {
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

        let _ = thread::Builder::new()
            .name("import-lens-recent-prewarm".to_owned())
            .spawn(move || {
                run_recent_prewarm_job(service, workspace_root, cancellation, generation);
            });
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
        package_json_prewarm_jobs(package_json_path, active_document_path)?
            .into_iter()
            .map(|job| job.request)
            .collect(),
    )
}

fn package_json_prewarm_jobs(
    package_json_path: &Path,
    active_document_path: &Path,
) -> Result<Vec<PrewarmJob>, String> {
    let contents = fs::read_to_string(package_json_path).map_err(|error| {
        format!(
            "failed to read package.json {}: {error}",
            package_json_path.display()
        )
    })?;
    let mut requests = Vec::new();

    for package_name in package_json_dependency_names(&contents)? {
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

// A `Default` prewarm job for a package with no `default` export emits an
// "exports" diagnostic, so `should_cache_result` refuses to cache the result and
// every prewarm trigger re-runs bundle+minify+compress for nothing. Suppress the
// Default variant when the entry exposes no default export -- but only when the
// module graph is ALREADY cached: building one here would serialize graph builds
// during enumeration and thrash the bounded GRAPH_CACHE on large manifests. On a
// cold cache this returns true (the Default job runs once, as before); a later
// prewarm, after the Namespace job has warmed the graph, then suppresses it.
fn exposes_default_export(resolved: &ResolvedPackage) -> bool {
    // CommonJS default interop always yields a usable default binding.
    if resolved.is_cjs {
        return true;
    }
    match cached_module_graph_with_runtime(&resolved.entry_path, ImportRuntime::Component) {
        Some(graph) => {
            module_provides_export(&graph, graph.entry_id, "default", &mut HashSet::new())
        }
        // Not cached yet -> don't suppress; the Default job runs this round.
        None => true,
    }
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

    let Ok(jobs) = package_json_prewarm_jobs(&package_json_path, &active_document_path) else {
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
    };

    let run = || {
        jobs.par_iter().for_each(|job| {
            if cancellation.is_current(generation) {
                service.prewarm_resolved_import(
                    &context,
                    &job.request,
                    job.resolved.clone(),
                    || cancellation.is_current(generation),
                );
            }
        });
    };

    if let Ok(pool) = prewarm_pool() {
        pool.install(run);
    }
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
    };
    let run = || {
        jobs.par_iter().for_each(|job| {
            if cancellation.is_current(generation) {
                service.prewarm_resolved_import(
                    &context,
                    &job.request,
                    job.resolved.clone(),
                    || cancellation.is_current(generation),
                );
            }
        });
    };

    if let Ok(pool) = prewarm_pool() {
        pool.install(run);
    }
}

fn installed_package(active_document_path: &Path, package_name: &str) -> Option<ResolvedPackage> {
    let request = prewarm_request(package_name, "", ImportKind::Namespace);
    resolve_package_entry(active_document_path, &request).ok()
}

pub fn cached_import_request_from_key(key: &str) -> Option<ImportRequest> {
    decode_cache_identity(key).map(import_request_from_identity)
}

fn import_request_from_identity(identity: CacheIdentityV3) -> ImportRequest {
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
