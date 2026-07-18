//! Bounded execution for post-build asset processing.

use rayon::{ThreadPool, ThreadPoolBuilder};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

const ASSET_PROCESSING_PERMITS: usize = 2;

/// An absolute deadline shared with the admitted job so long-running processors can stop
/// cooperatively instead of waiting for the boundary to discard their result.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AssetDeadline {
    started_at: Instant,
    limit: Duration,
}

impl AssetDeadline {
    fn new(limit: Duration) -> Self {
        Self {
            started_at: Instant::now(),
            limit,
        }
    }

    pub(crate) fn remaining(self) -> Duration {
        self.limit.saturating_sub(self.started_at.elapsed())
    }

    pub(crate) fn is_expired(self) -> bool {
        self.remaining().is_zero()
    }

    #[cfg(test)]
    pub(crate) fn for_test(limit: Duration) -> Self {
        Self::new(limit)
    }
}

/// Failures owned by the execution boundary rather than by the asset processor itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssetBoundaryError {
    AdmissionTimedOut { limit: Duration },
    ExecutionTimedOut { limit: Duration },
    Panicked { message: String },
    AdmissionFailed { message: String },
}

impl fmt::Display for AssetBoundaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdmissionTimedOut { limit } => write!(
                formatter,
                "asset processing was not admitted within {}s",
                limit.as_secs_f64()
            ),
            Self::ExecutionTimedOut { limit } => write!(
                formatter,
                "asset processing did not complete within {}s",
                limit.as_secs_f64()
            ),
            Self::Panicked { message } => write!(formatter, "asset processing panicked: {message}"),
            Self::AdmissionFailed { message } => {
                write!(formatter, "asset processing admission failed: {message}")
            }
        }
    }
}

impl std::error::Error for AssetBoundaryError {}

#[derive(Debug)]
struct Admission {
    available: Mutex<usize>,
    changed: Condvar,
}

impl Admission {
    fn new() -> Self {
        Self {
            available: Mutex::new(ASSET_PROCESSING_PERMITS),
            changed: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>, deadline: AssetDeadline) -> Result<Permit, AssetBoundaryError> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        loop {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(AssetBoundaryError::AdmissionTimedOut {
                    limit: deadline.limit,
                });
            }
            if *available > 0 {
                *available -= 1;
                return Ok(Permit {
                    admission: Arc::clone(self),
                });
            }

            let waited = self.changed.wait_timeout(available, remaining);
            let (next, _) = waited.unwrap_or_else(|poisoned| poisoned.into_inner());
            available = next;
        }
    }

    fn release(&self) {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *available += 1;
        debug_assert!(*available <= ASSET_PROCESSING_PERMITS);
        self.changed.notify_one();
    }
}

struct Permit {
    admission: Arc<Admission>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.admission.release();
    }
}

struct AssetExecutor {
    pool: ThreadPool,
    admission: Arc<Admission>,
}

impl AssetExecutor {
    fn new() -> Result<Self, AssetBoundaryError> {
        let pool = ThreadPoolBuilder::new()
            .num_threads(ASSET_PROCESSING_PERMITS)
            .thread_name(|index| format!("import-lens-asset-{index}"))
            .build()
            .map_err(|error| AssetBoundaryError::AdmissionFailed {
                message: error.to_string(),
            })?;
        Ok(Self {
            pool,
            admission: Arc::new(Admission::new()),
        })
    }

    fn submit<T, F>(&self, deadline: AssetDeadline, work: F) -> Result<T, AssetBoundaryError>
    where
        T: Send + 'static,
        F: FnOnce(AssetDeadline) -> T + Send + 'static,
    {
        let limit = deadline.limit;
        let permit = self.admission.acquire(deadline)?;
        if deadline.is_expired() {
            return Err(AssetBoundaryError::AdmissionTimedOut { limit });
        }
        let (sender, receiver) = mpsc::sync_channel(1);

        self.pool.spawn(move || {
            let _permit = permit;
            let outcome = if deadline.is_expired() {
                Err(AssetBoundaryError::ExecutionTimedOut { limit })
            } else {
                catch_unwind(AssertUnwindSafe(|| work(deadline))).map_err(|payload| {
                    let message = payload
                        .downcast_ref::<&'static str>()
                        .map(|text| (*text).to_owned())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "non-string panic payload".to_owned());
                    AssetBoundaryError::Panicked { message }
                })
            };
            let _ = sender.send(outcome);
        });

        match receiver.recv_timeout(deadline.remaining()) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(AssetBoundaryError::ExecutionTimedOut { limit })
            }
            Err(error @ mpsc::RecvTimeoutError::Disconnected) => {
                Err(AssetBoundaryError::Panicked {
                    message: format!("asset worker stopped without replying: {error}"),
                })
            }
        }
    }
}

fn executor() -> Result<&'static AssetExecutor, AssetBoundaryError> {
    static EXECUTOR: OnceLock<Result<AssetExecutor, AssetBoundaryError>> = OnceLock::new();
    match EXECUTOR.get_or_init(AssetExecutor::new) {
        Ok(executor) => Ok(executor),
        Err(error) => Err(error.clone()),
    }
}

/// Runs one post-build asset job on the dedicated pool. Admission is acquired before the job is
/// enqueued, so queued work cannot exceed the same two-job resource bound as running work.
///
/// The deadline starts when this function is entered and covers both admission and execution. A
/// timeout stops waiting for the result but does not cancel the closure: the worker retains its
/// permit until it returns, preventing abandoned work from widening actual resource concurrency.
pub(crate) fn execute<T, F>(limit: Duration, work: F) -> Result<T, AssetBoundaryError>
where
    T: Send + 'static,
    F: FnOnce(AssetDeadline) -> T + Send + 'static,
{
    let deadline = AssetDeadline::new(limit);
    let executor = executor()?;
    executor.submit(deadline, work)
}

#[cfg(test)]
mod tests {
    use super::{AssetDeadline, AssetExecutor};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    const TEST_LIMIT: Duration = Duration::from_secs(2);
    const WORKER_TIMEOUT_LIMIT: Duration = Duration::from_secs(1);
    const ADMISSION_PROBE_LIMIT: Duration = Duration::from_millis(150);

    fn executor() -> Arc<AssetExecutor> {
        Arc::new(AssetExecutor::new().expect("test asset executor should build"))
    }

    fn execute<T, F>(
        executor: &AssetExecutor,
        limit: Duration,
        work: F,
    ) -> Result<T, super::AssetBoundaryError>
    where
        T: Send + 'static,
        F: FnOnce(AssetDeadline) -> T + Send + 'static,
    {
        executor.submit(AssetDeadline::new(limit), work)
    }

    struct ReleaseGuard {
        released: Arc<AtomicBool>,
    }

    impl ReleaseGuard {
        fn new(released: Arc<AtomicBool>) -> Self {
            Self { released }
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
        }
    }

    impl Drop for ReleaseGuard {
        fn drop(&mut self) {
            self.release();
        }
    }

    #[test]
    fn asset_processing_never_exceeds_two_concurrent_jobs() {
        let executor = executor();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let release_guard = ReleaseGuard::new(Arc::clone(&released));
        let (started_sender, started_receiver) = mpsc::channel();

        let callers = (0..6)
            .map(|value| {
                let executor = Arc::clone(&executor);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let released = Arc::clone(&released);
                let started_sender = started_sender.clone();
                std::thread::spawn(move || {
                    execute(&executor, TEST_LIMIT, move |_| {
                        let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                        peak.fetch_max(current, Ordering::AcqRel);
                        started_sender
                            .send(())
                            .expect("the test should still be receiving starts");
                        // Nested rayon inside the job, because `compress.rs` and Lightning CSS both
                        // use it inside this pool. If a worker blocked in a join could steal a
                        // sibling job's task, real concurrency would exceed the permits and the
                        // peak below would catch it.
                        rayon::join(
                            || {
                                while !released.load(Ordering::Acquire) {
                                    std::thread::yield_now();
                                }
                            },
                            std::thread::yield_now,
                        );
                        active.fetch_sub(1, Ordering::AcqRel);
                        value
                    })
                })
            })
            .collect::<Vec<_>>();

        for _ in 0..2 {
            started_receiver
                .recv_timeout(TEST_LIMIT)
                .expect("two asset jobs should be admitted");
        }
        assert_eq!(active.load(Ordering::Acquire), 2);
        release_guard.release();

        let mut values = callers
            .into_iter()
            .map(|caller| {
                caller
                    .join()
                    .expect("boundary callers should not panic")
                    .expect("bounded work should complete")
            })
            .collect::<Vec<_>>();
        values.sort_unstable();

        assert_eq!(values, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(peak.load(Ordering::Acquire), 2);
    }

    #[test]
    fn timed_out_jobs_retain_their_permits_until_the_workers_exit() {
        let executor = executor();
        let released = Arc::new(AtomicBool::new(false));
        let release_guard = ReleaseGuard::new(Arc::clone(&released));
        let (started_sender, started_receiver) = mpsc::channel();
        let (outcome_sender, outcome_receiver) = mpsc::channel();

        let callers = (0..2)
            .map(|_| {
                let executor = Arc::clone(&executor);
                let released = Arc::clone(&released);
                let started_sender = started_sender.clone();
                let outcome_sender = outcome_sender.clone();
                std::thread::spawn(move || {
                    let outcome = execute(&executor, WORKER_TIMEOUT_LIMIT, move |_| {
                        started_sender
                            .send(())
                            .expect("the test should still be receiving starts");
                        while !released.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                    });
                    let _ = outcome_sender.send(outcome);
                })
            })
            .collect::<Vec<_>>();

        for _ in 0..2 {
            started_receiver
                .recv_timeout(TEST_LIMIT)
                .expect("both jobs should start before their deadlines");
        }

        let mut timed_out = Vec::new();
        for _ in 0..2 {
            match outcome_receiver.recv_timeout(TEST_LIMIT) {
                Ok(outcome) => timed_out.push(outcome),
                Err(error) => {
                    release_guard.release();
                    for caller in callers {
                        let _ = caller.join();
                    }
                    panic!("boundary did not return at the execution deadline: {error}");
                }
            }
        }
        for caller in callers {
            caller.join().expect("boundary caller should not panic");
        }
        assert!(timed_out.iter().all(|outcome| matches!(
            outcome,
            Err(super::AssetBoundaryError::ExecutionTimedOut { .. })
        )));

        let third_ran = Arc::new(AtomicBool::new(false));
        let third_ran_from_job = Arc::clone(&third_ran);
        let admission = execute(&executor, ADMISSION_PROBE_LIMIT, move |_| {
            third_ran_from_job.store(true, Ordering::Release);
        });
        assert!(matches!(
            admission,
            Err(super::AssetBoundaryError::AdmissionTimedOut { .. })
        ));
        assert!(!third_ran.load(Ordering::Acquire));

        release_guard.release();
        assert_eq!(execute(&executor, TEST_LIMIT, |_| 7), Ok(7));
    }

    #[test]
    fn a_panicking_asset_job_returns_a_typed_error_and_the_pool_recovers() {
        let executor = executor();

        let failure = execute::<(), _>(&executor, TEST_LIMIT, |_| panic!("synthetic asset panic"))
            .expect_err("a panicking job must not produce a value");
        let super::AssetBoundaryError::Panicked { message } = failure else {
            panic!("panic should retain its boundary type: {failure:?}");
        };
        assert!(message.contains("synthetic asset panic"), "{message}");

        assert_eq!(execute(&executor, TEST_LIMIT, |_| "healthy"), Ok("healthy"));
    }
}
