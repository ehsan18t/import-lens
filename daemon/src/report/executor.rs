use rayon::{ThreadPool, ThreadPoolBuilder};

const MAX_REPORT_WORKER_THREADS: usize = 4;

pub struct WorkspaceReportExecutor {
    pool: ThreadPool,
}

impl WorkspaceReportExecutor {
    pub fn new() -> Self {
        let pool = ThreadPoolBuilder::new()
            .num_threads(default_report_worker_threads())
            .thread_name(|index| format!("import-lens-report-{index}"))
            .build()
            .expect("workspace report thread pool should build");
        Self { pool }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.pool.spawn(job);
    }

    pub fn install<R: Send>(&self, job: impl FnOnce() -> R + Send) -> R {
        self.pool.install(job)
    }
}

impl Default for WorkspaceReportExecutor {
    fn default() -> Self {
        Self::new()
    }
}

fn default_report_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|count| (count.get() / 2).clamp(1, MAX_REPORT_WORKER_THREADS))
        .unwrap_or(2)
}
