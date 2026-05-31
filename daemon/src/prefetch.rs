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
    let contents = fs::read_to_string(package_json_path).map_err(|error| {
        format!(
            "failed to read package.json {}: {error}",
            package_json_path.display()
        )
    })?;
    let mut requests = Vec::new();

    for package_name in package_json_dependency_names(&contents)? {
        let Some(version) = installed_package_version(active_document_path, &package_name) else {
            continue;
        };

        requests.push(prewarm_request(
            &package_name,
            &version,
            ImportKind::Default,
        ));
        requests.push(prewarm_request(
            &package_name,
            &version,
            ImportKind::Namespace,
        ));
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

    let Ok(requests) = package_json_prewarm_requests(&package_json_path, &active_document_path)
    else {
        return;
    };

    if requests.is_empty() || !cancellation.is_current(generation) {
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
        requests.par_iter().for_each(|request| {
            if cancellation.is_current(generation) {
                service.prewarm_import(&context, request, || cancellation.is_current(generation));
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

fn installed_package_version(active_document_path: &Path, package_name: &str) -> Option<String> {
    let request = prewarm_request(package_name, "", ImportKind::Namespace);
    let ResolvedPackage { package_json, .. } =
        resolve_package_entry(active_document_path, &request).ok()?;

    package_json
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
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
