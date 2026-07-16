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
        "the boundary call never returned: a build that panicked or was cancelled leaked its \
         permit, and the daemon is wedged",
    )
}

/// How long a synthetic parked build may hold its permit: the test's stand-in for
/// `BUILD_TIMEOUT`, so the tests play out in milliseconds what production plays out in seconds.
const PARK_LIMIT: Duration = Duration::from_millis(300);

fn park() -> String {
    let failure = within_deadline(move || bundle_sync_for_test_hang(PARK_LIMIT))
        .expect_err("a build that never completes must return a failure, not park the caller");
    failure.stage
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
/// This is the ONE bound the design keeps. It bounds a build, so that a permit is never held
/// forever; it does not bound a request, and no longer needs to, because no request waits on a
/// build (`service::handle_analyze_document_streaming`).
#[test]
fn a_parked_build_times_out_and_gives_its_permit_back() {
    let _guard = serialized();

    for _ in 0..ENGINE_PERMITS {
        assert_eq!(
            park(),
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

/// A parked build must delay ITS OWN import and nothing else.
///
/// This is the half of the guarantee the boundary owns. The other half is
/// `service::handle_analyze_document_streaming` starting no builds at all, so a response can
/// never be behind one (`document_streaming.rs`); together they are what makes a parked package
/// cost one import's number instead of a whole document's.
///
/// It is deliberately not a *request* bound: nothing here is cancelled, abandoned, or degraded on
/// account of some other build being slow. The parked build runs out its own clock while a build
/// beside it finishes on the second permit, undisturbed.
#[test]
fn a_parked_build_does_not_delay_the_build_beside_it() {
    let _guard = serialized();
    let before = builds_started();

    // One build parks, holding one of the two permits for the whole of PARK_LIMIT.
    let parked = std::thread::spawn(|| bundle_sync_for_test_hang(PARK_LIMIT));
    // Wait until it is genuinely in flight: `builds_started` increments INSIDE the permit, so
    // this is the point past which the permit is actually held. Without it the sibling below
    // might run before the parked build has taken anything, and prove nothing.
    let admitted_at = Instant::now();
    while builds_started() == before {
        assert!(
            admitted_at.elapsed() < PARK_LIMIT,
            "the parked build never acquired a permit"
        );
        std::thread::yield_now();
    }

    let started_at = Instant::now();
    let sibling = within_deadline(bundle_sync_for_test_panic)
        .expect_err("the synthetic sibling build panics");
    let elapsed = started_at.elapsed();

    assert_eq!(sibling.stage, "panic", "the sibling build ran to its end");
    assert!(
        elapsed < PARK_LIMIT,
        "a build beside a parked one must not wait for it: {elapsed:?}"
    );
    assert!(
        !parked.is_finished(),
        "the parked build must still be parked — otherwise this proved nothing"
    );
    assert_eq!(
        parked
            .join()
            .expect("a boundary caller must not panic")
            .expect_err("a parked build cannot produce an artifact")
            .stage,
        "timeout",
        "the parked build ends on its own clock, not on anybody else's"
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
