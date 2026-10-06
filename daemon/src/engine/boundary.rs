//! Async execution boundary (spec §9): at most two Rolldown builds are in
//! flight daemon-wide, and synchronous analysis threads reach the async
//! engine through a dedicated runtime owned here. Cache hits never touch
//! this module; only misses pay for a permit.
//!
//! Size-producing service and prewarm loops feed this boundary through the
//! bounded miss drain (`scheduling`), preserving final input order without
//! parking the global Rayon pool.

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use super::{BundleArtifact, BundleFailure, BundleRequest, ImportRuntime, RolldownEngine, stage};

/// Spec §9: two concurrent builds bound peak memory while keeping one slow
/// build from serializing the daemon. The miss drain sizes its worker count
/// from this.
///
/// The memory bound is approximate after a `BUILD_TIMEOUT`: the permit is released at once,
/// but Rolldown's already-spawned module tasks keep the abandoned graph resident until they
/// finish, so peak RSS can briefly reach ~3 graphs (known issue C2).
pub const ENGINE_PERMITS: usize = 2;

/// Upper bound on a single engine build.
///
/// **It exists so a permit is never held forever, not to police slowness.** A panic inside one
/// of Rolldown's `tokio::spawn`ed module tasks is swallowed by Tokio: the task never sends its
/// `*Done` message and the loader (which holds its own sender clone) waits forever. Nothing
/// unwinds, so `catch_unwind` cannot see it, and `ENGINE_PERMITS` such builds would wedge every
/// later build. Dropping the timed-out future releases the permit and the `InFlight` guard, and
/// the import degrades to one typed `timeout` failure (known issue C1).
///
/// **It bounds a build, not a request.** An interactive request answers from the cache and each
/// build is pushed to the client as it lands (`ipc::server`, `RefreshedResults`), so a parked
/// build delays only its own import. Do not add a request-scoped engine budget: it degrades
/// healthy packages and caches the degraded numbers.
///
/// 8s is 16x the §10.6 cold-p95 gate (500 ms) and ~160x the measured cold p95 (52 ms): a build
/// that reaches it is pathological. It is flat across `BundlePurpose`, since no purpose
/// identifies a deadline (`ImportSize` serves both the interactive path and the workspace
/// report).
const BUILD_TIMEOUT: Duration = Duration::from_secs(8);

static PERMITS: Semaphore = Semaphore::const_new(ENGINE_PERMITS);
/// The engine permits background builds may hold at once; see [`run_as_background`].
static BACKGROUND_PERMITS: Semaphore = Semaphore::const_new(ENGINE_PERMITS - 1);
const _: () = assert!(
    ENGINE_PERMITS >= 2,
    "a permit must remain for interactive builds"
);
static WAITING: AtomicUsize = AtomicUsize::new(0);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static PEAK_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicUsize = AtomicUsize::new(0);
// Borrowing a static keeps the engine futures 'static for Runtime::spawn.
static ENGINE: RolldownEngine = RolldownEngine;

/// Runtime width decides how fast each admitted build goes (Rolldown parallelizes within a
/// build); `ENGINE_PERMITS` alone bounds how many builds run, and so peak memory. Do not size
/// the runtime to the permit count. Capped at 8: past that the daemon contends with the editor
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
            // Rolldown reads every module through `spawn_blocking` here. Tokio's default of 512
            // lets one build grow 80 to 150 reader threads, each keeping an allocator heap until
            // its keep-alive expires; the reads are short, so queueing them costs nothing. Never
            // `block_in_place` on this runtime: against a capped pool it can deadlock.
            .max_blocking_threads(engine_runtime_workers())
            .thread_name("il-engine")
            .enable_all()
            .build()
            .expect("engine runtime should build")
    })
}

/// Decrements on drop, so a build future dropped before it finishes (the `BUILD_TIMEOUT`
/// cancellation, runtime shutdown) cannot leak the counter.
struct InFlight;

impl InFlight {
    fn enter() -> Self {
        STARTED.fetch_add(1, Ordering::Relaxed);
        let current = IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
        PEAK_IN_FLIGHT.fetch_max(current, Ordering::Relaxed);
        Self
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Counts a build from submission to admission; decrements on drop for the same reason
/// `InFlight` does.
struct Waiting;

impl Waiting {
    fn enter() -> Self {
        WAITING.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        WAITING.fetch_sub(1, Ordering::Relaxed);
    }
}

thread_local! {
    static BACKGROUND: Cell<bool> = const { Cell::new(false) };
}

/// Run `work` with every engine build it starts on this thread admitted as background work.
///
/// Prewarm runs under this. A background build may hold at most `ENGINE_PERMITS - 1` permits,
/// so however much prewarm is queued, one permit is always held by or free for an interactive
/// build: a user's import never queues behind prewarm builds holding every permit. The miss
/// drain carries the mark onto its worker threads.
pub fn run_as_background<R>(work: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            BACKGROUND.with(|background| background.set(self.0));
        }
    }
    let _restore = Restore(BACKGROUND.with(|background| background.replace(true)));
    work()
}

pub(crate) fn is_background() -> bool {
    BACKGROUND.with(Cell::get)
}

/// Everything that has to happen inside the permit: the in-flight guard, the build timeout, and
/// the `catch_unwind`. However the build ends (value, unwind, or cancellation), the permit and
/// the guard are released before this returns.
async fn with_permit<T>(
    cap: Duration,
    background: bool,
    work: impl Future<Output = Result<T, BundleFailure>>,
) -> Result<T, BundleFailure> {
    let waiting = Waiting::enter();
    // Always background-then-engine, and interactive builds take only the engine permit, so
    // no cycle can form. Both semaphores are FIFO, so a queued background build is admitted
    // in turn and cannot be starved by a stream of interactive builds.
    let _background = if background {
        Some(
            BACKGROUND_PERMITS
                .acquire()
                .await
                .expect("background permit semaphore is never closed"),
        )
    } else {
        None
    };
    let _permit = PERMITS
        .acquire()
        .await
        .expect("engine permit semaphore is never closed");
    drop(waiting);

    let _in_flight = InFlight::enter();
    match tokio::time::timeout(cap, AssertUnwindSafe(work).catch_unwind()).await {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) => Err(panic_failure(&payload)),
        Err(_elapsed) => Err(timeout_failure(cap)),
    }
}

/// Submit work to the engine runtime and block the calling thread until it completes.
///
/// Every outcome, including a Rolldown or OXC panic, is a typed `BundleFailure` for *this*
/// import, never a panic on the calling analysis thread (which would lose the whole batch).
/// A panic that never unwinds is covered by `BUILD_TIMEOUT`.
fn run_on_engine<T: Send + 'static>(
    cap: Duration,
    work: impl Future<Output = Result<T, BundleFailure>> + Send + 'static,
) -> Result<T, BundleFailure> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let background = is_background();
    engine_runtime().spawn(async move {
        let outcome = with_permit(cap, background, work).await;
        let _ = sender.send(outcome);
    });

    // The sender is dropped without a send only if the engine runtime itself is gone. That is
    // still this import's failure, not a panic, so it does not inflate the panic count.
    receiver.recv().unwrap_or_else(|_| {
        Err(BundleFailure {
            stage: stage::ENGINE_GONE.to_owned(),
            message: "the engine runtime dropped the build without replying".to_owned(),
            diagnostics: Vec::new(),
            loaded_paths: Vec::new(),
            read_time_fingerprints: Vec::new(),
        })
    })
}

/// Rust panic payloads are `&str` for a literal `panic!` and `String` for a formatted one;
/// anything else is opaque.
fn panic_failure(payload: &(dyn std::any::Any + Send)) -> BundleFailure {
    let detail = payload
        .downcast_ref::<&'static str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());

    BundleFailure {
        stage: stage::PANIC.to_owned(),
        message: format!("engine build panicked: {detail}"),
        diagnostics: Vec::new(),
        loaded_paths: Vec::new(),
        read_time_fingerprints: Vec::new(),
    }
}

/// A build that never completed. The overwhelmingly likely cause is a panic inside one of
/// the module tasks Rolldown spawns: Tokio swallows it, the task never reports done, and
/// the loader waits on a message that will never arrive.
fn timeout_failure(limit: Duration) -> BundleFailure {
    BundleFailure {
        stage: stage::TIMEOUT.to_owned(),
        message: format!(
            "engine build did not complete within {}s; this usually means a module task \
             panicked inside the bundler and the build never finished",
            limit.as_secs_f64()
        ),
        diagnostics: Vec::new(),
        loaded_paths: Vec::new(),
        read_time_fingerprints: Vec::new(),
    }
}

/// Run one bundle build behind the daemon-wide permit pool, from a synchronous caller.
///
/// Admission control and the build limit are owned here, not carried on the request: §5 keeps
/// `BundleRequest` a description of what to build.
pub fn bundle_sync(request: BundleRequest) -> Result<BundleArtifact, BundleFailure> {
    run_on_engine(BUILD_TIMEOUT, ENGINE.bundle(request))
}

/// Synchronous export enumeration through the same permit pool and build limit (§8.4). It
/// builds the same package graph as a size build, so it can park the same way.
pub fn enumerate_exports_sync(
    entry_path: PathBuf,
    runtime: ImportRuntime,
) -> Result<super::ExportEnumeration, BundleFailure> {
    run_on_engine(BUILD_TIMEOUT, ENGINE.enumerate_exports(entry_path, runtime))
}

/// Highest number of builds ever observed in flight; the boundary's
/// integration test asserts this never exceeds the permit count.
pub fn peak_in_flight() -> usize {
    PEAK_IN_FLIGHT.load(Ordering::Relaxed)
}

/// Total engine builds admitted through the permit pool since start: the unit tests use to
/// prove a change stopped doing work.
pub fn builds_started() -> usize {
    STARTED.load(Ordering::Relaxed)
}

/// Builds submitted to the boundary and not yet admitted. Lets the admission test know every
/// build it started has reached the semaphores before it measures who gets in.
#[doc(hidden)]
pub fn builds_waiting() -> usize {
    WAITING.load(Ordering::Relaxed)
}

/// Drives a build future that panics, through the real permit/runtime path. Exists so the
/// isolation guarantee is tested against the boundary that ships, not a mock.
#[doc(hidden)]
pub fn bundle_sync_for_test_panic() -> Result<BundleArtifact, BundleFailure> {
    run_on_engine(BUILD_TIMEOUT, async { panic!("synthetic engine panic") })
}

/// Drives a build future that never completes, through the real permit/runtime path. This is
/// what a panic inside a Rolldown-spawned module task looks like from here: no unwind, no value,
/// just a parked future holding a permit.
///
/// `cap` stands in for `BUILD_TIMEOUT` so a test runs in milliseconds; the path is otherwise
/// identical to `bundle_sync` (same permit, timeout, and counters).
#[doc(hidden)]
pub fn bundle_sync_for_test_hang(cap: Duration) -> Result<BundleArtifact, BundleFailure> {
    run_on_engine(
        cap,
        std::future::pending::<Result<BundleArtifact, BundleFailure>>(),
    )
}
