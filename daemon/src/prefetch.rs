use crate::{
    ipc::protocol::{ImportKind, ImportRequest},
    pipeline::{
        analyze::AnalysisContext,
        resolver::{ResolvedPackage, resolve_package_entry},
    },
    service::ImportLensService,
};
use rayon::{ThreadPoolBuilder, prelude::*};
use serde_json::Value;
use std::{
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

pub fn package_json_dependency_names(contents: &str) -> Result<Vec<String>, String> {
    let json = serde_json::from_str::<Value>(contents)
        .map_err(|error| format!("failed to parse package.json: {error}"))?;
    let mut names = Vec::new();

    for field in ["dependencies", "devDependencies"] {
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

        requests.push(PrewarmJob {
            request: prewarm_request(&package_name, &version, ImportKind::Default),
            resolved: resolved.clone(),
        });
        requests.push(PrewarmJob {
            request: prewarm_request(&package_name, &version, ImportKind::Namespace),
            resolved,
        });
    }

    Ok(requests)
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

    let pool = PREWARM_POOL.get_or_init(|| {
        ThreadPoolBuilder::new()
            .num_threads(prewarm_thread_count())
            .build()
            .expect("failed to build prewarm thread pool")
    });

    pool.install(run);
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

    let active_document_path = workspace_root.join("package.json");
    let jobs = service
        .recent_cache_keys(RECENT_PREWARM_LIMIT)
        .into_iter()
        .filter_map(|key| cached_import_request_from_key(&key))
        .filter_map(|request| {
            let resolved = resolve_package_entry(&active_document_path, &request).ok()?;
            Some(PrewarmJob { request, resolved })
        })
        .collect::<Vec<_>>();

    if jobs.is_empty() || !cancellation.is_current(generation) {
        return;
    }

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

    let pool = PREWARM_POOL.get_or_init(|| {
        ThreadPoolBuilder::new()
            .num_threads(prewarm_thread_count())
            .build()
            .expect("failed to build prewarm thread pool")
    });

    pool.install(run);
}

fn installed_package(active_document_path: &Path, package_name: &str) -> Option<ResolvedPackage> {
    let request = prewarm_request(package_name, "", ImportKind::Namespace);
    resolve_package_entry(active_document_path, &request).ok()
}

pub fn cached_import_request_from_key(key: &str) -> Option<ImportRequest> {
    let (specifier_and_version, exports) = key.split_once("::")?;
    let version_separator = specifier_and_version.rfind('@')?;
    if version_separator == 0 {
        return None;
    }

    let specifier = &specifier_and_version[..version_separator];
    let version = &specifier_and_version[version_separator + 1..];
    if specifier.is_empty() || version.is_empty() || exports.is_empty() {
        return None;
    }

    let (import_kind, named) = match exports {
        "default" => (ImportKind::Default, Vec::new()),
        "*" => (ImportKind::Namespace, Vec::new()),
        "dynamic" => (ImportKind::Dynamic, Vec::new()),
        named => (
            ImportKind::Named,
            named
                .split(',')
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
    };

    Some(ImportRequest {
        specifier: specifier.to_owned(),
        package_name: package_name_from_specifier(specifier),
        version: version.to_owned(),
        named,
        import_kind,
    })
}

fn package_name_from_specifier(specifier: &str) -> String {
    if specifier.starts_with('@') {
        let mut parts = specifier.split('/');
        return match (parts.next(), parts.next()) {
            (Some(scope), Some(name)) => format!("{scope}/{name}"),
            _ => specifier.to_owned(),
        };
    }

    specifier
        .split('/')
        .next()
        .map(str::to_owned)
        .unwrap_or_else(|| specifier.to_owned())
}

fn prewarm_request(package_name: &str, version: &str, import_kind: ImportKind) -> ImportRequest {
    ImportRequest {
        specifier: package_name.to_owned(),
        package_name: package_name.to_owned(),
        version: version.to_owned(),
        named: Vec::new(),
        import_kind,
    }
}

fn prewarm_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|value| (value.get() / 2).max(1))
        .unwrap_or(1)
}
