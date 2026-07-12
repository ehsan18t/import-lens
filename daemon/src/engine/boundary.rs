//! Async execution boundary (spec §9): at most two Rolldown builds are in
//! flight daemon-wide, and synchronous analysis threads reach the async
//! engine through a dedicated runtime owned here. Cache hits never touch
//! this module; only misses pay for a permit.
//!
//! Size-producing service and prewarm loops feed this boundary through the
//! two-worker scheduler, preserving final input order without parking the
//! global Rayon pool.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use super::{
    BundleArtifact, BundleFailure, BundleRequest, EngineBudget, ImportRuntime, RolldownEngine,
    stage,
};

/// Spec §9: two concurrent builds bound peak memory while keeping one slow
/// build from serializing the daemon. Public so miss-draining loops size
/// their worker count to the permit count instead of parking extra threads.
///
/// The memory bound is exact for builds that *finish*, and approximate for one that
/// hits `BUILD_TIMEOUT`. Dropping a timed-out build future releases its permit at once,
/// but the module tasks Rolldown already `tokio::spawn`ed keep running: they hold an
/// `Arc` of the build's context (and the module sources they parsed), and this boundary
/// has no join handle with which to wait for them. So while an abandoned graph is still
/// resident, two fresh builds can be admitted and peak RSS can briefly reach ~3 graphs
/// rather than 2. It is bounded — those tasks do complete and drop their state — and it
/// can neither wedge the pool nor corrupt a result. Fixing it would mean tracking and
/// joining Rolldown's internal tasks, which its public surface does not offer; the
/// honest thing is to record the approximation here rather than to overstate the bound.
pub const ENGINE_PERMITS: usize = 2;

/// Upper bound on a single engine build.
///
/// **It exists to bound a hang, not to police slowness.** `catch_unwind` only sees a panic
/// that unwinds *to us*, and the panic that matters does not. Rolldown fans every module out
/// onto its own `tokio::spawn`ed task (`module_loader.rs`), and a panic in one of those tasks
/// is swallowed by Tokio: the task dies without sending its `*Done` message, the loader's
/// `remaining` counter never reaches zero, and — because the loader itself holds a clone of
/// the message sender — its `rx.recv()` never returns `None` either. The build future parks
/// forever. Nothing unwinds, so nothing is caught; the permit and the in-flight guard are
/// never released, and `ENGINE_PERMITS` such packages wedge every later build in the daemon's
/// lifetime. Dropping the timed-out future is the containment: it releases the permit and the
/// `InFlight` guard, and the import degrades to one typed `timeout` failure.
///
/// **This value MUST stay below the tightest client deadline it serves.** The extension's
/// interactive requests give up after 10s (`extension/src/ipc/client.ts`, default
/// `timeoutMs = 10000`) and none of the analyze/exports/completions/file-size callers raise
/// it. A build timeout above that deadline contains nothing: the parked build still outlives
/// the client's patience, and the extension rejects the *entire* `AnalyzeDocumentResponse` —
/// including every import in that document already answered from cache.
///
/// **It bounds one build, and only one build.** A permit is acquired *outside* it, so parked
/// builds queue rather than merely run late, and enough of them serialize a single request past
/// its client deadline however short this value is. That bound belongs to the request, not to
/// the build: see [`super::budget`], whose deadline every build here is also held to, and which
/// is re-checked *after* the permit precisely because a build can sit in that queue for as long
/// as the builds ahead of it park. This constant is the hard ceiling on a build that is admitted;
/// the request's remaining budget can only make it shorter.
///
/// 8s is 16x the §10.6 cold-p95 gate (500 ms) and ~160x the measured cold p95 (52 ms): a build
/// that reaches it is pathological by construction. The limit is deliberately flat and not
/// varied by `BundlePurpose` — no purpose identifies a deadline (`ImportSize` serves both the
/// 10s interactive path and the 300s workspace report), which is exactly why the deadline is
/// threaded down from the IPC request layer, the only layer that knows it.
const BUILD_TIMEOUT: Duration = Duration::from_secs(8);

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

/// Decrements on drop, so a build that never *completes* cannot leak the counter.
/// `catch_unwind` sits inside the permit, so nothing unwinds through this guard — what
/// it protects against is the future being dropped before it finishes: the
/// `BUILD_TIMEOUT` cancellation above, and runtime shutdown. The semaphore permit
/// already self-cleans on drop; the counters did not.
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

/// Everything that has to happen inside the permit: the deadline re-check, the in-flight
/// guard, the build timeout, and the `catch_unwind`. However the build ends — value, unwind,
/// or cancellation — the permit and the guard are released before this returns.
async fn with_permit<T>(
    budget: EngineBudget,
    cap: Duration,
    work: impl Future<Output = Result<T, BundleFailure>>,
) -> Result<T, BundleFailure> {
    let _permit = PERMITS
        .acquire()
        .await
        .expect("engine permit semaphore is never closed");

    // The re-check, and the reason the budget exists at all. A build that waited here while the
    // builds ahead of it parked has spent the request's time doing nothing, and admitting it now
    // would start a FRESH `cap` clock — which is how one document used to serialize into two
    // build timeouts and lose its whole response, cached hits included. It abandons instead.
    //
    // Before `InFlight::enter`, deliberately: a build that never ran must not be counted as
    // started, or `builds_started()` stops being able to tell us whether this works.
    let Some(limit) = budget.build_limit(cap) else {
        return Err(budget_spent_failure());
    };

    let _in_flight = InFlight::enter();
    match tokio::time::timeout(limit, AssertUnwindSafe(work).catch_unwind()).await {
        Ok(Ok(result)) => result,
        Ok(Err(payload)) => Err(panic_failure(&payload)),
        Err(_elapsed) => Err(timeout_failure(limit)),
    }
}

/// Submit work to the engine runtime and block the calling thread until it completes.
///
/// The build future is wrapped in `catch_unwind`: a Rolldown or OXC panic that unwinds to
/// us becomes a typed `BundleFailure` for *this* import. Before this, a panicking task
/// dropped the channel sender, `recv()` returned `Err`, and the `expect` panicked the
/// calling analysis thread — destroying the whole batch, including every import already
/// answered from cache.
///
/// The build limit covers the panic that never unwinds at all: one inside a module task
/// Rolldown spawned, which parks the build forever. See `BUILD_TIMEOUT`.
fn run_on_engine<T: Send + 'static>(
    budget: EngineBudget,
    cap: Duration,
    work: impl Future<Output = Result<T, BundleFailure>> + Send + 'static,
) -> Result<T, BundleFailure> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    engine_runtime().spawn(async move {
        let outcome = with_permit(budget, cap, work).await;
        let _ = sender.send(outcome);
    });

    // The sender is dropped without a send only if the engine runtime itself is gone.
    // That is not recoverable, but it is still THIS import's failure, not the calling
    // thread's panic -- which is the entire point of this function. It is not a panic,
    // so it does not get to inflate the panic count.
    receiver.recv().unwrap_or_else(|_| {
        Err(BundleFailure {
            stage: stage::ENGINE_GONE.to_owned(),
            message: "the engine runtime dropped the build without replying".to_owned(),
            diagnostics: Vec::new(),
            loaded_paths: Vec::new(),
        })
    })
}

/// Rust panic payloads are `&str` for a literal `panic!` and `String` for a formatted one;
/// anything else is opaque. Name what we can and stay honest about the rest.
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
    }
}

/// A build that was never started because the request had no engine time left (`super::budget`).
///
/// It reports the same `timeout` stage as a build that ran out of time, because from the caller's
/// side these are one event: an engine build that could not produce a number inside the deadline
/// the client is waiting on. Both degrade the same way.
fn budget_spent_failure() -> BundleFailure {
    BundleFailure {
        stage: stage::TIMEOUT.to_owned(),
        message: "this request had no engine time left, so the build was not started: earlier \
                  builds in the same request did not complete within their limit, and running \
                  another one would only push the response past the deadline the client is \
                  waiting on"
            .to_owned(),
        diagnostics: Vec::new(),
        loaded_paths: Vec::new(),
    }
}

/// The request's engine budget plus the permit pool: the whole path from a synchronous caller
/// into the engine, so no caller can take one guard without the other.
fn guarded<T: Send + 'static>(
    budget: EngineBudget,
    cap: Duration,
    work: impl Future<Output = Result<T, BundleFailure>> + Send + 'static,
) -> Result<T, BundleFailure> {
    // Checked *before* queueing for a permit as well as after acquiring one. A request with
    // nothing left to spend must not even join the queue: it returns now, and the caller falls
    // back to static sizing, which needs no permit and no engine.
    if budget.build_limit(cap).is_none() {
        return Err(budget_spent_failure());
    }

    run_on_engine(budget, cap, work)
}

/// Run one bundle build behind the daemon-wide permit pool, from a synchronous caller, within
/// what is left of the calling request's engine budget.
///
/// The budget is a *parameter* rather than a field of `BundleRequest` on purpose. §5 keeps
/// `BundleRequest` a description of **what to build** — entries, runtime, purpose — which
/// `adapter.rs` turns into an artifact. A deadline is not an input to a build; it is admission
/// control, owned by this boundary, and the engine has no business reading it. Putting it in the
/// request would push a scheduling concern through every engine-facing type and let a future
/// adapter make a build's *result* depend on how long its caller had been waiting.
pub fn bundle_sync(
    request: BundleRequest,
    budget: EngineBudget,
) -> Result<BundleArtifact, BundleFailure> {
    guarded(budget, BUILD_TIMEOUT, ENGINE.bundle(request))
}

/// Synchronous export enumeration through the same permit pool and the same request budget
/// (§8.4). An enumeration builds the same package graph as a size build, so it parks on exactly
/// the same module task and must be held to exactly the same bound.
pub fn enumerate_exports_sync(
    entry_path: PathBuf,
    runtime: ImportRuntime,
    budget: EngineBudget,
) -> Result<super::ExportEnumeration, BundleFailure> {
    guarded(
        budget,
        BUILD_TIMEOUT,
        ENGINE.enumerate_exports(entry_path, runtime),
    )
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

/// Drives a build future that panics, through the real budget/permit/runtime path. Exists so
/// the isolation guarantee is tested against the boundary that ships, not a mock.
#[doc(hidden)]
pub fn bundle_sync_for_test_panic() -> Result<BundleArtifact, BundleFailure> {
    guarded(EngineBudget::interactive(), BUILD_TIMEOUT, async {
        panic!("synthetic engine panic")
    })
}

/// Drives a build future that never completes, through the real budget/permit/runtime path.
/// This is what a panic inside a Rolldown-spawned module task looks like from here: no unwind,
/// no value, just a parked future holding a permit.
///
/// The caller supplies both limits so a test can play out in milliseconds what production plays
/// out in seconds: `cap` stands in for `BUILD_TIMEOUT`, and `budget` for the deadline the request
/// arrived with. The code path is otherwise identical to `bundle_sync` — same `guarded`, same
/// pre-permit check, same post-permit re-check, same counters.
#[doc(hidden)]
pub fn bundle_sync_for_test_hang(
    cap: Duration,
    budget: EngineBudget,
) -> Result<BundleArtifact, BundleFailure> {
    guarded(
        budget,
        cap,
        std::future::pending::<Result<BundleArtifact, BundleFailure>>(),
    )
}
