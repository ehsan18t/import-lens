//! Async execution boundary (spec §9): at most two Rolldown builds are in
//! flight daemon-wide, and synchronous analysis threads reach the async
//! engine through a dedicated runtime owned here. Cache hits never touch
//! this module; only misses pay for a permit.
//!
//! Size-producing service and prewarm loops feed this boundary through the
//! two-worker scheduler, preserving final input order without parking the
//! global Rayon pool.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use super::{BundleArtifact, BundleFailure, BundleRequest, ImportRuntime, RolldownEngine};

/// Spec §9: two concurrent builds bound peak memory while keeping one slow
/// build from serializing the daemon. Public so miss-draining loops size
/// their worker count to the permit count instead of parking extra threads.
pub const ENGINE_PERMITS: usize = 2;

static PERMITS: Semaphore = Semaphore::const_new(ENGINE_PERMITS);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static PEAK_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicUsize = AtomicUsize::new(0);
// Borrowing a static keeps the engine futures 'static for Runtime::spawn.
static ENGINE: RolldownEngine = RolldownEngine;

/// Rolldown parallelizes *within* a build — parsing, transforming and rendering
/// modules across the runtime's workers. Sizing the runtime to `ENGINE_PERMITS`
/// conflated two unrelated bounds: the permits exist to cap how many builds run at
/// once (and so peak memory), while the runtime width decides how fast each of those
/// builds can go. Two workers meant every build was pinned to two threads no matter
/// how many cores the machine had.
///
/// The permits still bound concurrency and memory; this only lets each admitted build
/// use the machine. Capped at 8: past that the daemon would contend with the editor
/// and the Rayon pool for cores it cannot productively use.
fn engine_runtime_workers() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().clamp(ENGINE_PERMITS, 8))
        .unwrap_or(ENGINE_PERMITS)
}

/// The engine runtime is separate from the IPC runtime so `bundle_sync` can
/// be called from rayon/service threads (which are never Tokio workers)
/// without deadlocking the I/O executor.
fn engine_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(engine_runtime_workers())
            .thread_name("il-engine")
            .enable_all()
            .build()
            .expect("engine runtime should build")
    })
}

async fn with_permit<T>(work: impl Future<Output = T>) -> T {
    let _permit = PERMITS
        .acquire()
        .await
        .expect("engine permit semaphore is never closed");
    STARTED.fetch_add(1, Ordering::Relaxed);
    let current = IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
    PEAK_IN_FLIGHT.fetch_max(current, Ordering::Relaxed);
    let output = work.await;
    IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    output
}

/// Submit work to the engine runtime and block the calling thread on a
/// plain channel until it completes. `Runtime::spawn` is legal from any
/// thread — including `spawn_blocking` threads that already carry the IPC
/// runtime's context, where `block_on` would panic — and the analysis
/// threads doing the waiting are rayon or blocking-pool threads, never
/// Tokio workers.
fn run_on_engine<T: Send + 'static>(work: impl Future<Output = T> + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    engine_runtime().spawn(async move {
        let _ = sender.send(with_permit(work).await);
    });
    receiver
        .recv()
        .expect("the engine runtime should always reply")
}

/// Run one bundle build behind the daemon-wide permit pool, from a
/// synchronous caller.
pub fn bundle_sync(request: BundleRequest) -> Result<BundleArtifact, BundleFailure> {
    run_on_engine(ENGINE.bundle(request))
}

/// Synchronous export enumeration through the same permit pool (§8.4).
pub fn enumerate_exports_sync(
    entry_path: PathBuf,
    runtime: ImportRuntime,
) -> Result<Vec<String>, BundleFailure> {
    run_on_engine(ENGINE.enumerate_exports(entry_path, runtime))
}

/// Highest number of builds ever observed in flight; the boundary's
/// integration test asserts this never exceeds the permit count.
pub fn peak_in_flight() -> usize {
    PEAK_IN_FLIGHT.load(Ordering::Relaxed)
}

/// Total engine builds admitted through the permit pool since start. A build is
/// the single most expensive thing the daemon does, so the count is the honest
/// unit for "did that change actually stop doing work" — it is what the
/// full-package memo's regression test measures.
pub fn builds_started() -> usize {
    STARTED.load(Ordering::Relaxed)
}
