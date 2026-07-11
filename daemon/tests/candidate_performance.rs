//! Candidate-engine measurement harness (spec §10.6). Release-only and
//! explicitly ignored, mirroring daemon/tests/performance.rs and its
//! IMPORT_LENS_PERF_MULTIPLIER tolerance:
//!
//! ```text
//! node scripts/prepare-candidate-fixtures.mjs
//! # set IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints, then:
//! cargo test -p import-lens-daemon --release --locked \
//!     --test candidate_performance -- \
//!     --ignored --nocapture
//! ```
//!
//! Startup, idle RSS, and the cache-hit path are intentionally NOT measured
//! here: the candidate feature is not compiled into the shipped binary and
//! the default dependency graph was verified unchanged when the dependency
//! landed, so those §10.6 gates are governed by the existing release
//! baselines until Part C wires the engine into the service.

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, ImportRuntime, RolldownEngine,
};
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod common;

fn threshold_ms(base_ms: u128) -> u128 {
    let multiplier = env::var("IMPORT_LENS_PERF_MULTIPLIER")
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .unwrap_or(6)
        .max(1);

    base_ms * multiplier
}

fn p95_of(durations: &mut [Duration]) -> Duration {
    durations.sort();
    let index = (durations.len() * 95).div_ceil(100).saturating_sub(1);
    durations[index]
}

/// Peak (not current) working set of THIS process. Tests in one binary share
/// the process, so the reading can only overstate a single scenario — the
/// conservative direction for an upper-bound gate.
#[cfg(windows)]
fn peak_working_set_bytes() -> u64 {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id()),
        ])
        .output()
        .expect("powershell should be available for the peak-RSS probe");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("PeakWorkingSet64 should be numeric")
}

#[cfg(not(windows))]
fn peak_working_set_bytes() -> u64 {
    let status =
        std::fs::read_to_string("/proc/self/status").expect("/proc/self/status should be readable");
    let kilobytes: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .expect("VmHWM should be present and numeric");
    kilobytes * 1024
}

async fn bundle_once(entry: BundleEntry) -> usize {
    let artifact = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![entry],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
        .expect("fixture bundle should succeed");
    artifact.code.len()
}

// §10.6 hard gate: single typical cold import p95 ≤ 500 ms. Every run builds
// a fresh bundler (no reuse across requests), so each is a genuinely cold
// engine pass; the OS file cache stays warm, as it would in the daemon.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn cold_single_import_p95_stays_under_release_threshold() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let resolve =
        || common::engine_fixtures::resolve_fixture_entry(&workspace, "css-tree", "3.2.1", "parse");

    for _ in 0..5 {
        bundle_once(resolve()).await;
    }
    let mut durations = Vec::with_capacity(30);
    let mut raw_bytes = 0usize;
    for _ in 0..30 {
        let entry = resolve();
        let start = Instant::now();
        raw_bytes = bundle_once(entry).await;
        durations.push(start.elapsed());
    }
    let p50 = durations[durations.len() / 2];
    let p95 = p95_of(&mut durations);
    let max = *durations.last().expect("30 runs recorded");
    eprintln!(
        "candidate cold css-tree/parse: p50 {p50:?}, p95 {p95:?}, max {max:?} over 30 runs \
         ({raw_bytes} raw bytes)"
    );

    assert!(
        p95.as_millis() <= threshold_ms(500),
        "cold single import p95 exceeded the 500 ms gate: {}ms",
        p95.as_millis()
    );
}

// §10.6 hard gate: a 20-import active batch stays below 400 MB peak RSS.
// The batch matches the spec's comparison set: independent packages, shared
// transitive dependencies, a CJS package, and repeated different exports
// from single packages. Concurrency is capped at two builds in flight — the
// execution-boundary shape Part C introduces.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn twenty_import_batch_peak_rss_stays_under_release_threshold() {
    const BATCH: &[(&str, &str, &str)] = &[
        ("css-tree", "3.2.1", "parse"),
        ("css-tree", "3.2.1", "generate"),
        ("css-tree", "3.2.1", "walk"),
        ("date-fns", "4.1.0", "format"),
        ("date-fns", "4.1.0", "addDays"),
        ("date-fns", "4.1.0", "parseISO"),
        ("date-fns", "4.1.0", "subDays"),
        ("lodash-es", "4.18.1", "debounce"),
        ("lodash-es", "4.18.1", "throttle"),
        ("lodash-es", "4.18.1", "cloneDeep"),
        ("lodash-es", "4.18.1", "merge"),
        ("lodash", "4.17.21", "debounce"),
        ("zod", "4.4.3", "z"),
        ("zod", "4.4.3", "ZodError"),
        ("react", "19.2.7", "useState"),
        ("react", "19.2.7", "useEffect"),
        ("react", "19.2.7", "useMemo"),
        ("uuid", "14.0.1", "v4"),
        ("uuid", "14.0.1", "v1"),
        ("uuid", "14.0.1", "validate"),
    ];

    let workspace = common::engine_fixtures::fixtures_workspace();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(2));
    let batch_start = Instant::now();
    let mut handles = Vec::with_capacity(BATCH.len());
    for (package, version, export) in BATCH {
        let entry =
            common::engine_fixtures::resolve_fixture_entry(&workspace, package, version, export);
        let permits = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let _permit = permits.acquire().await.expect("semaphore should stay open");
            bundle_once(entry).await
        }));
    }
    let mut total_raw_bytes = 0usize;
    for handle in handles {
        total_raw_bytes += handle.await.expect("batch task should not panic");
    }
    let elapsed = batch_start.elapsed();
    let peak = peak_working_set_bytes();

    eprintln!(
        "candidate 20-import batch: {elapsed:?} wall clock, peak RSS {} MB, \
         {total_raw_bytes} total raw bytes",
        peak / (1024 * 1024)
    );
    assert!(
        peak < 400 * 1024 * 1024,
        "20-import batch peak RSS exceeded the 400 MB gate: {peak} bytes"
    );
}
