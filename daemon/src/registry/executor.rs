use rayon::{ThreadPool, ThreadPoolBuilder};

pub struct RegistryRefreshExecutor {
    pool: ThreadPool,
}

impl RegistryRefreshExecutor {
    pub fn new(thread_count: usize) -> Self {
        let pool = ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .thread_name(|index| format!("import-lens-registry-{index}"))
            .build()
            .expect("registry refresh thread pool should build");
        Self { pool }
    }

    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.pool.spawn(job);
    }
}
