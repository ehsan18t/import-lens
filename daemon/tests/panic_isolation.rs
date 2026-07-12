//! The release profile must unwind.
//!
//! The daemon isolates panics with `catch_unwind`: a panicking workspace file is
//! skipped from the report rather than failing the scan, a panicking analysis returns
//! an error response, and the report and registry workers survive. Under
//! `panic = "abort"` none of that works — the panic runtime aborts the process on the
//! spot, `catch_unwind` never runs, and one bad file takes the user's daemon down.
//!
//! The release profile carried `panic = "abort"` through the entire bundler redesign,
//! which meant every one of those guards was dead code in the shipped binary while
//! every test that covers them passed. They passed because **Cargo ignores
//! `panic = "abort"` for the test profile** — the isolation tests are compiled with
//! unwinding no matter what the release profile says, so they can never catch this.
//!
//! That is what makes this guard necessary rather than decorative: it is the only
//! thing in the suite that can fail if someone sets `panic = "abort"` again to shave
//! binary size.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use import_lens_daemon::engine::EngineBudget;
use import_lens_daemon::engine::boundary::{
    ENGINE_PERMITS, builds_started, bundle_sync_for_test_hang, bundle_sync_for_test_panic,
    peak_in_flight,
};

/// The `[profile.release]` section of the workspace manifest, as raw lines.
fn release_profile() -> Vec<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the daemon crate sits inside the workspace")
        .join("Cargo.toml");
    let contents = fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("workspace manifest should be readable: {error}"));

    contents
        .lines()
        .skip_while(|line| line.trim() != "[profile.release]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .map(|line| line.trim().to_owned())
        .collect()
}

#[test]
fn the_release_profile_does_not_abort_on_panic() {
    let profile = release_profile();
    assert!(
        !profile.is_empty(),
        "[profile.release] should exist in the workspace manifest"
    );

    let panic_setting = profile
        .iter()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix("panic"))
        .map(|value| value.trim_start_matches(['=', ' ']).trim().to_owned());

    assert_ne!(
        panic_setting.as_deref(),
        Some("\"abort\""),
        "release must unwind: the daemon's eight catch_unwind isolation sites are dead \
         code under panic = \"abort\", and no other test can catch this because Cargo \
         ignores the setting for the test profile"
    );
}

/// The guard above is only worth having while the isolation it protects still exists.
/// If every `catch_unwind` were removed, the profile setting would stop mattering and
/// this file should go with it.
#[test]
fn the_daemon_still_relies_on_catch_unwind() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sites = 0;

    let mut pending = vec![source_root];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).expect("daemon source tree should be readable");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = fs::read_to_string(&path).expect("source file should be readable");
                sites += source.matches("catch_unwind").count();
            }
        }
    }

    assert!(
        sites > 0,
        "no catch_unwind left in the daemon — if panic isolation is genuinely gone, \
         delete this file; otherwise something was removed by mistake"
    );
}

/// `IN_FLIGHT`/`PEAK_IN_FLIGHT` are process-global, and cargo runs the tests in one
/// binary on parallel threads. Without this, one test's build can be admitted while
/// another test is mid-measurement, latching a peak neither test expected.
static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every call into the boundary in this file goes through here.
///
/// A leaked permit does not make a boundary call fail — it makes it block on `acquire()`
/// forever. Cargo has no per-test timeout and libtest prints results only after the last test
/// returns, so a call made *inline* turns that regression into a CI job that hangs with no
/// output at all. On its own thread behind a `recv_timeout`, the same regression is a red test
/// with a message that says what broke.
fn within_deadline<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });

    receiver.recv_timeout(Duration::from_secs(10)).expect(
        "the boundary call never returned: a build that panicked, was cancelled, or was \
         abandoned for want of budget leaked its permit, and the daemon is wedged",
    )
}

/// How long a synthetic parked build may hold its permit: the test's stand-in for
/// `BUILD_TIMEOUT`, so the tests play out in milliseconds what production plays out in seconds.
const PARK_LIMIT: Duration = Duration::from_millis(300);

fn park(budget: EngineBudget) -> String {
    let failure = within_deadline(move || bundle_sync_for_test_hang(PARK_LIMIT, budget))
        .expect_err("a build that never completes must return a failure, not park the caller");
    failure.stage
}

/// Submit `count` parked builds **at the same time**, on their own threads, all sharing one
/// request's budget — the shape a real document produces.
///
/// Concurrency is the whole point. A document's misses are drained on `MISS_DRAIN_WORKERS`
/// (= `ENGINE_PERMITS + 2`) scoped threads (`engine::scheduling`), so the builds past the permit
/// count do not arrive *after* the earlier ones fail — they are already **queued on the
/// semaphore** while those park. That queued state is where the defect lived, and a sequential
/// submission loop cannot reach it: each of its calls returns before the next begins, so it only
/// ever sees an interleaving production never produces. The previous covering test looped
/// sequentially and was green against code that still lost the document.
fn parked_document(count: usize, budget: EngineBudget) -> (Vec<String>, Duration, usize) {
    let before = builds_started();
    let started_at = Instant::now();

    let stages = std::thread::scope(|scope| {
        let handles = (0..count)
            .map(|_| scope.spawn(move || park(budget)))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a boundary caller must not panic"))
            .collect::<Vec<_>>()
    });

    (stages, started_at.elapsed(), builds_started() - before)
}

/// A panic inside a build must degrade that one build, not the caller. Before the
/// `catch_unwind`, `run_on_engine`'s `recv().expect(...)` panicked the *calling*
/// thread, which unwound through `thread::scope` and failed the whole batch.
#[test]
fn engine_panic_becomes_a_typed_failure_not_a_caller_panic() {
    let _guard = serialized();

    let failure = within_deadline(bundle_sync_for_test_panic)
        .expect_err("a panicking build must return a failure, not unwind the caller");

    assert_eq!(failure.stage, "panic");
    assert!(
        failure.message.contains("engine build panicked"),
        "message should name the panic: {}",
        failure.message
    );
}

/// The in-flight counter must not leak when a build unwinds: `peak_in_flight()` is the
/// only assertion of the §9 two-build invariant, so a latched counter silently disables
/// the daemon's sole concurrency check.
#[test]
fn a_panicking_build_does_not_leak_the_in_flight_counter() {
    let _guard = serialized();
    let before = peak_in_flight();

    for _ in 0..3 {
        let _ = within_deadline(bundle_sync_for_test_panic);
    }

    assert_eq!(
        peak_in_flight(),
        before.max(1),
        "peak must not climb with each panicking build"
    );
}

/// The panic that actually threatens the daemon never unwinds to us. Rolldown runs each
/// module on its own spawned Tokio task; a panic there is swallowed by Tokio, the task
/// never reports done, and the loader — which holds its own clone of the message sender,
/// so `recv()` never returns `None` — waits forever. `catch_unwind` cannot see it: nothing
/// unwinds. The build future simply parks, holding a permit.
///
/// Without the timeout, `ENGINE_PERMITS` such packages consume every permit permanently and
/// **every** later build in the daemon's lifetime blocks on `acquire()` — a wedge that only a
/// restart clears. So the load-bearing assertion here is the last one: that the daemon is
/// still able to run a build after enough parked builds to have exhausted the pool.
///
/// These builds carry a *background* budget — prewarm's — so nothing but the per-build limit can
/// end them: the point here is that cancelling a parked build hands its permit back, and a budget
/// that abandoned the later ones would test the wrong thing.
#[test]
fn a_parked_build_times_out_and_gives_its_permit_back() {
    let _guard = serialized();

    for _ in 0..ENGINE_PERMITS {
        assert_eq!(
            park(EngineBudget::background()),
            "timeout",
            "a build that never completes must fail as a timeout"
        );
    }

    // Every permit has now been held by a build that was cancelled rather than completed. If
    // cancellation did not release them, this call blocks on `acquire()` forever.
    let failure =
        within_deadline(bundle_sync_for_test_panic).expect_err("the synthetic build panics");
    assert_eq!(
        failure.stage, "panic",
        "a build after the timeouts must still be admitted through the permit pool"
    );
}

/// The defect the request budget exists to close, reproduced.
///
/// `BUILD_TIMEOUT` bounds a build, and a permit is acquired *outside* it — so parked builds do
/// not merely run late, they QUEUE. Three imports of one broken package fill both permits and
/// park; the third sits blocked on the semaphore *inside* the boundary. When the first two are
/// cancelled and release their permits, the third is admitted and starts a **fresh** build clock.
/// Two build timeouts of engine time for one document, the extension gives up at 10s, and the
/// rejected `AnalyzeDocumentResponse` takes every import in that document already answered from
/// cache with it. The old per-entry circuit breaker could not see this: it was checked at
/// submission, when nothing had parked yet, and never again.
///
/// So the third build re-checks the request's deadline **after** it acquires its permit and
/// abandons without building. Both assertions matter and neither is sufficient alone:
/// `builds_started()` is the honest count of builds that really reached the engine (it increments
/// inside the permit, past the re-check), and the wall clock is what the client actually
/// experiences — a fix that skipped the build but still waited would pass the first and fail the
/// second.
#[test]
fn a_build_queued_behind_parked_ones_abandons_instead_of_starting_a_fresh_clock() {
    let _guard = serialized();

    // Budget and per-build cap deliberately equal: the two builds that get permits spend the
    // whole request budget, so the one that queued behind them has nothing left when it is
    // admitted. (Production's 9s budget over an 8s cap leaves the third build a 1s remnant it
    // may spend — still inside the deadline, which is all the budget promises.)
    let (stages, elapsed, admitted) =
        parked_document(ENGINE_PERMITS + 1, EngineBudget::expiring_in(PARK_LIMIT));

    assert!(
        stages.iter().all(|stage| stage == "timeout"),
        "every import of a parked package must degrade with the typed timeout failure: {stages:?}"
    );
    assert_eq!(
        admitted, ENGINE_PERMITS,
        "only the builds that could start inside the request's budget may reach the engine; the \
         one that queued on the semaphore while they parked must abandon on admission, not start \
         a fresh build clock"
    );
    assert!(
        elapsed < PARK_LIMIT * 2,
        "the document must cost ONE build timeout of engine time, not two: {elapsed:?}"
    );
}

/// The case a per-entry circuit breaker could never bound: a document naming MORE distinct broken
/// packages than there are permits. Keyed by entry, the breaker had no record for a package that
/// had not parked yet, so every distinct one bought another full build timeout and the document
/// serialized past the deadline anyway.
///
/// The boundary now keys nothing by entry — it has no memory of what parked at all — so these six
/// parked builds stand for six *different* broken packages exactly as well as for six imports of
/// one. What bounds them is the budget they share, which is the request's.
#[test]
fn more_broken_packages_than_permits_still_cost_one_build_timeout() {
    let _guard = serialized();

    let (stages, elapsed, admitted) =
        parked_document(ENGINE_PERMITS + 4, EngineBudget::expiring_in(PARK_LIMIT));

    assert!(
        stages.iter().all(|stage| stage == "timeout"),
        "every import must degrade with the typed timeout failure: {stages:?}"
    );
    assert_eq!(
        admitted, ENGINE_PERMITS,
        "engine time is bounded by the request's budget, not by how many broken packages the \
         document names"
    );
    assert!(
        elapsed < PARK_LIMIT * 2,
        "six parked builds on two permits must still cost ONE build timeout, not three waves of \
         one: {elapsed:?}"
    );
}

/// A request that has already spent its budget must not even queue for a permit: it returns the
/// typed failure at once and the caller degrades to the static fallback (`analyze.rs`), which
/// needs no engine.
#[test]
fn a_request_with_no_engine_time_left_never_enters_the_engine() {
    let _guard = serialized();
    let before = builds_started();

    let stage = park(EngineBudget::expiring_in(Duration::ZERO));

    assert_eq!(stage, "timeout", "a spent budget reports the timeout stage");
    assert_eq!(
        builds_started(),
        before,
        "a request with nothing left to spend must not start a build"
    );
}

/// The stage vocabulary lives in `engine::stage`/`engine::diagnostic_stage` precisely so a new
/// stage cannot be introduced without landing in `stage::ALL`, which is what `contract_stage`
/// derives from. A bare literal at a construction site bypasses that and is silently relabelled
/// `generate` at the contract edge while `file_size.rs` passes it through untouched — one
/// failure reaching the user under two names, which is the original bug.
///
/// Scans the whole engine, not just `boundary.rs`: `adapter.rs` constructs `BundleFailure` at
/// three sites and was unguarded. rustfmt puts every struct field on its own line, so a
/// reintroduced literal is always on a line of its own.
#[test]
fn the_engine_names_its_stages_from_the_shared_vocabulary() {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("engine");

    let mut literals = Vec::new();
    let entries = fs::read_dir(&engine).expect("the engine source directory should be readable");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("engine source file should be readable");

        for line in source.lines().map(str::trim) {
            let Some(value) = line.strip_prefix("stage:") else {
                continue;
            };
            let value = value.trim_start();
            // A stage built out of thin air rather than named: a literal, or a `String` or
            // `format!` that produces one.
            if value.starts_with('"')
                || value.starts_with("String::from")
                || value.starts_with("format!")
            {
                literals.push(format!("{}: {line}", path.display()));
            }
        }
    }

    assert!(
        literals.is_empty(),
        "every stage in daemon/src/engine must come from engine::stage (whose ALL list drives \
         contract_stage) or engine::diagnostic_stage, not from a string built at the \
         construction site: {literals:#?}"
    );
}
