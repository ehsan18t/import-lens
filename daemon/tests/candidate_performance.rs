//! The §10.6 runtime performance and memory gates, over the pinned REAL packages. Release-only and
//! explicitly ignored, mirroring daemon/tests/performance.rs:
//!
//! ```text
//! node scripts/prepare-candidate-fixtures.mjs
//! # set IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints, then:
//! cargo test -p import-lens-daemon --release --locked \
//!     --test candidate_performance -- \
//!     --ignored --nocapture --test-threads=1
//! ```
//!
//! **Until 2026-07-14 nothing invoked this file.** It is `#[ignore]`d and needs installed fixtures,
//! and no workflow step and no package.json script named it — while `pnpm test:performance`, which
//! CI *does* call, runs `daemon/tests/performance.rs`: the legacy suite over synthetic fixtures, a
//! different file. So a gate appeared to run and did not. `validate.yml` runs it now, on the same
//! installed fixtures as `candidate_packages`, on every pull request.
//!
//! **Every gate here is measured against the SHIPPED DAEMON BINARY, over the real IPC transport,
//! over a real package.** That is not ceremony. Each §10.6 number is a claim about the process the
//! user runs, and an in-process `ImportLensService` is not that process:
//!
//! - a cold import is not `RolldownEngine::bundle`. A cache miss also resolves the specifier, runs
//!   a *second* engine build (the full-package comparison behind `truly_treeshakeable`), minifies
//!   through OXC, runs three compressors, fingerprints, and writes the cache. The first version of
//!   this file gated the engine build alone — 22 ms — and let the other ~107 ms of the real cold
//!   path (measured below) regress untouched. NFR-003 is about the whole miss;
//! - an in-process service construction is not a startup (NFR-005);
//! - the RSS of a cargo-test process that has just bundled twenty packages in-process is not the
//!   daemon's RSS (NFR-004). Both memory gates read the spawned daemon's own working set.
//!
//! The engine build survives as a *diagnostic* at the bottom of the file: when the cold gate goes
//! red, it says whether the engine moved or the pipeline around it did.
//!
//! `IMPORT_LENS_PERF_MULTIPLIER` **defaults to 1** — the literal §10.6 numbers. A default above 1
//! means no run anywhere, local or CI, ever enforces the requirement that was written down; the
//! default used to be 6, so none ever did. CI opts *up*, by a measured amount, and says why in
//! `validate.yml`.
//!
//! It scales the two gates that measurably need CPU headroom on a shared runner, and **nothing
//! else**. Measured on this file's own fixtures with the process pinned to 4 logical cores (a
//! GitHub `ubuntu-24.04` runner is 4 vCPU) and those 4 cores 2x oversubscribed with competing load
//! — deliberately harsher than a dedicated runner — against the literal gates:
//!
//! | gate | hostile-CI p95 | literal gate | margin |
//! | --- | --- | --- | --- |
//! | cold import | 406 ms | 500 ms | 1.23x — thin, needs the multiplier |
//! | daemon startup | 77 ms | 500 ms | 6.5x |
//! | cache-hit response | 19 ms | 50 ms | 2.6x — holds, so it is NOT scaled |
//! | idle RSS | 20 MB | 100 MB | 5.0x |
//! | 20-import batch RSS | 86 MB | 400 MB | 4.7x |
//!
//! So the memory gates stay absolute, and **so does the cache-hit gate**: NFR-002's 50 ms is
//! Critical, it survives the hostile emulation with 2.6x to spare, and multiplying it — CI used to
//! run at 8, making it 400 ms — turns a hard requirement into a number no one chose. If it ever
//! does go red on CI, that is a fact about NFR-002 worth hearing, not a number to inflate.

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, ImportRuntime, RolldownEngine,
};
use import_lens_daemon::ipc::codec::{FrameDecoder, decode_payload, encode_frame};
use import_lens_daemon::ipc::protocol::{
    BatchRequest, BatchResponse, HelloMessage, ImportKind, ImportRequest, PROTOCOL_VERSION,
};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

mod common;

/// §10.6: "five warm-up runs followed by at least 30 recorded runs".
const WARMUP_RUNS: usize = 5;
const RECORDED_RUNS: usize = 30;

/// The one import every latency gate is measured on. `css-tree` is the deep-ESM-graph fixture of
/// the §10.3 real-package set, and `parse` is one export of it — a typical named import, which is
/// what NFR-003 sizes. Holding the cold gate and the engine diagnostic to the *same* import is what
/// makes the two numbers subtractable.
const LATENCY_PACKAGE: &str = "css-tree";
const LATENCY_EXPORT: &str = "parse";

fn threshold_ms(base_ms: u128) -> u128 {
    let multiplier = env::var("IMPORT_LENS_PERF_MULTIPLIER")
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .unwrap_or(1)
        .max(1);

    base_ms * multiplier
}

fn p95_of(durations: &mut [Duration]) -> Duration {
    durations.sort();
    let index = (durations.len() * 95).div_ceil(100).saturating_sub(1);
    durations[index]
}

/// p50 / p95 / max of a recorded run, printed and then gated on p95.
struct Percentiles {
    p50: Duration,
    p95: Duration,
    max: Duration,
}

fn percentiles_of(durations: &mut [Duration]) -> Percentiles {
    assert_eq!(
        durations.len(),
        RECORDED_RUNS,
        "§10.6 requires at least 30 recorded runs"
    );
    durations.sort();
    Percentiles {
        p50: durations[durations.len() / 2],
        p95: p95_of(durations),
        max: *durations.last().expect("recorded runs are non-empty"),
    }
}

/// Peak (high-water) working set of the SPAWNED DAEMON. NFR-004's 400 MB batch ceiling is a claim
/// about the daemon process, so it is read from the daemon process — the cargo-test binary that
/// drives it is not the thing under test, and it shares one process across every test in this file.
#[cfg(windows)]
fn peak_working_set_bytes(process_id: u32) -> u64 {
    windows_process_metric(process_id, "PeakWorkingSet64")
}

#[cfg(not(windows))]
fn peak_working_set_bytes(process_id: u32) -> u64 {
    proc_status_kilobytes(process_id, "VmHWM:") * 1024
}

/// Current (not peak) working set of the spawned daemon: NFR-004's idle-RSS half.
#[cfg(windows)]
fn working_set_bytes(process_id: u32) -> u64 {
    windows_process_metric(process_id, "WorkingSet64")
}

#[cfg(not(windows))]
fn working_set_bytes(process_id: u32) -> u64 {
    proc_status_kilobytes(process_id, "VmRSS:") * 1024
}

#[cfg(windows)]
fn windows_process_metric(process_id: u32, property: &str) -> u64 {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {process_id}).{property}"),
        ])
        .output()
        .expect("powershell should be available for the RSS probe");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|error| {
            panic!("{property} of pid {process_id} should be numeric: {error} — is it still alive?")
        })
}

#[cfg(not(windows))]
fn proc_status_kilobytes(process_id: u32, field: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{process_id}/status"))
        .expect("/proc/<pid>/status should be readable — is the daemon still alive?");
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{field} should be present and numeric"))
}

#[cfg(windows)]
type DaemonStream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(not(windows))]
type DaemonStream = tokio::net::UnixStream;

/// A spawned daemon, connected and greeted, with its own storage directory.
///
/// The `Drop` kills it and removes the storage however the test ends: a panicking gate must not
/// leave a daemon behind holding the pipe, and — because the cold gate spawns a fresh daemon per
/// run — must not leave 35 redb databases behind either.
struct DaemonSession {
    child: std::process::Child,
    stream: DaemonStream,
    decoder: FrameDecoder,
    storage: PathBuf,
    /// Spawn → the moment the daemon accepted an IPC connection. NFR-005 word for word.
    startup: Duration,
}

impl DaemonSession {
    fn process_id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for DaemonSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.storage);
    }
}

fn endpoint_name() -> String {
    let unique = format!(
        "import-lens-candidate-perf-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos()
    );
    if cfg!(windows) {
        format!(r"\\.\pipe\{unique}")
    } else {
        std::env::temp_dir()
            .join(format!("{unique}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(windows)]
async fn connect(endpoint: &str) -> DaemonStream {
    use tokio::net::windows::named_pipe::ClientOptions;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return client,
            Err(error) if Instant::now() >= deadline => {
                panic!("daemon never accepted a connection on {endpoint}: {error}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
        }
    }
}

#[cfg(not(windows))]
async fn connect(endpoint: &str) -> DaemonStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::net::UnixStream::connect(endpoint).await {
            Ok(stream) => return stream,
            Err(error) if Instant::now() >= deadline => {
                panic!("daemon never accepted a connection on {endpoint}: {error}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
        }
    }
}

/// Spawns the shipped binary in the shipped configuration — disk cache on, in a storage directory
/// of its own — and returns once it has answered a connection and been greeted.
async fn start_daemon(workspace: &Path) -> DaemonSession {
    let storage = common::temp_workspace("import-lens-candidate-storage");
    let endpoint = endpoint_name();

    let launched = Instant::now();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_import-lens-daemon"))
        .args([
            "--pipe",
            &endpoint,
            "--workspace",
            &workspace.to_string_lossy(),
            "--storage",
            &storage.to_string_lossy(),
        ])
        .spawn()
        .expect("the shipped daemon binary should start");
    let mut stream = connect(&endpoint).await;
    let startup = launched.elapsed();

    send(
        &mut stream,
        &HelloMessage {
            message_type: "hello".to_owned(),
            version: PROTOCOL_VERSION,
            workspace_root: workspace.to_string_lossy().into_owned(),
            storage_path: storage.to_string_lossy().into_owned(),
            enable_disk_cache: true,
            cache_max_size_mb: 200,
            registry_cache_max_size_mb: 50,
            log_level: "error".to_owned(),
        },
    )
    .await;

    DaemonSession {
        child,
        stream,
        decoder: FrameDecoder::default(),
        storage,
        startup,
    }
}

async fn send<S, T>(stream: &mut S, message: &T)
where
    S: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let frame = encode_frame(message).expect("client frame should encode");
    stream
        .write_all(&frame)
        .await
        .expect("client frame should be writable");
    stream.flush().await.expect("client frame should flush");
}

async fn read_batch_response<S>(stream: &mut S, decoder: &mut FrameDecoder) -> BatchResponse
where
    S: AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(60), stream.read(&mut buffer))
            .await
            .expect("daemon should answer within 60s")
            .expect("daemon response should be readable");
        assert!(read > 0, "daemon closed the connection before responding");

        for payload in decoder
            .push(&buffer[..read])
            .expect("daemon frame should decode")
        {
            if let Ok(response) = decode_payload::<BatchResponse>(&payload) {
                return response;
            }
        }
    }
}

/// One request/response round trip, timed the way the user experiences it: from the moment the
/// request leaves to the moment the answer is decoded.
async fn timed_batch(
    session: &mut DaemonSession,
    request: &BatchRequest,
) -> (BatchResponse, Duration) {
    let start = Instant::now();
    send(&mut session.stream, request).await;
    let response = read_batch_response(&mut session.stream, &mut session.decoder).await;
    (response, start.elapsed())
}

fn named_import(workspace: &Path, package: &str, export: &str) -> ImportRequest {
    ImportRequest {
        specifier: package.to_owned(),
        package_name: package.to_owned(),
        // Read from the installed manifest, never typed here: a pinned version is a fact about
        // scripts/accuracy-fixtures/package.json, and repeating it would add a place to forget.
        version: common::pipeline_fixtures::installed_version(workspace, package),
        named: vec![export.to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::default(),
    }
}

fn batch_of(workspace: &Path, request_id: u64, imports: Vec<ImportRequest>) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().into_owned(),
        active_document_path: workspace
            .join("src")
            .join("app.ts")
            .to_string_lossy()
            .into_owned(),
        imports,
        streaming: false,
    }
}

fn latency_batch(workspace: &Path, request_id: u64) -> BatchRequest {
    batch_of(
        workspace,
        request_id,
        vec![named_import(workspace, LATENCY_PACKAGE, LATENCY_EXPORT)],
    )
}

/// A gate that timed a FAILED analysis would be timing the error path, and an Unmeasured result
/// never enters the engine at all — it would make every gate here trivially green.
fn assert_measured(response: &BatchResponse, expected: usize) {
    assert_eq!(
        response.imports.len(),
        expected,
        "the daemon answered a different number of imports than it was asked"
    );
    for result in &response.imports {
        assert!(
            result.sizes().is_some(),
            "every import must be MEASURED for these timings to mean anything — `{}` is unmeasured \
             (stage: {:?}, error: {:?})",
            result.specifier,
            result.unmeasured_stage(),
            result.error,
        );
    }
}

// NFR-003 (Critical, §10.6): a single typical cold import — a CACHE MISS — has p95 ≤ 500 ms.
// NFR-005 (High, §10.6): the daemon accepts connections within 500 ms of being spawned.
//
// A cold import is measured END TO END, through the shipped daemon: specifier resolution, the
// engine build, the full-package comparison build, OXC minification, gzip/Brotli/zstd, the
// fingerprint, and the cache write — the work a user's cache miss actually pays for. The previous
// version of this gate called `RolldownEngine::bundle` directly and asserted on that, which is one
// of those steps.
//
// Every run gets a FRESH DAEMON with a FRESH STORAGE DIRECTORY, which is the only way to keep 30
// runs genuinely cold: the daemon memoizes the full-package build, the export list and file sizes
// in process, so a second miss against a package the same daemon already touched is a
// partially-warm path wearing a cold name. Startup rides along on the same 30 spawns for free —
// which is also the first time NFR-005 has been read from more than a single sample.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn shipped_daemon_cold_import_p95_and_startup_stay_under_release_thresholds() {
    let workspace = common::engine_fixtures::fixtures_workspace();

    let mut cold = Vec::with_capacity(RECORDED_RUNS);
    let mut startup = Vec::with_capacity(RECORDED_RUNS);
    for run in 0..(WARMUP_RUNS + RECORDED_RUNS) {
        let mut session = start_daemon(&workspace).await;
        let (miss, elapsed) = timed_batch(&mut session, &latency_batch(&workspace, 1)).await;

        assert_measured(&miss, 1);
        assert!(
            !miss.imports[0].cache_hit,
            "a fresh daemon on a fresh storage directory must MISS: {:?}",
            miss.imports[0],
        );

        if run >= WARMUP_RUNS {
            cold.push(elapsed);
            startup.push(session.startup);
        }
    }

    let cold = percentiles_of(&mut cold);
    let startup = percentiles_of(&mut startup);
    eprintln!(
        "shipped daemon cold {LATENCY_PACKAGE}/{LATENCY_EXPORT} (end to end, {RECORDED_RUNS} runs): \
         p50 {:?}, p95 {:?}, max {:?}\nshipped daemon startup ({RECORDED_RUNS} spawns): p50 {:?}, \
         p95 {:?}, max {:?}",
        cold.p50, cold.p95, cold.max, startup.p50, startup.p95, startup.max,
    );

    assert!(
        cold.p95.as_millis() <= threshold_ms(500),
        "cold single import p95 exceeded the 500 ms gate (NFR-003): {}ms",
        cold.p95.as_millis()
    );
    assert!(
        startup.p95.as_millis() <= threshold_ms(500),
        "daemon startup p95 exceeded the 500 ms gate (NFR-005): {}ms",
        startup.p95.as_millis()
    );
}

// NFR-002 (Critical, §10.6): cache-hit response stays under 50 ms. ABSOLUTE — see the module
// header: this gate is not scaled by IMPORT_LENS_PERF_MULTIPLIER, because it does not need to be.
// NFR-004 (High, §10.6): idle RSS with the cache populated stays under 100 MB.
//
// One daemon, one cold request to populate the cache, then 30 recorded identical requests over the
// same connection. 50 ms is a hard number in the SRS, so it is read as a p95 over 30 round trips
// rather than the single sample this used to take.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn shipped_daemon_cache_hit_p95_and_idle_rss_stay_under_release_thresholds() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let mut session = start_daemon(&workspace).await;

    let (miss, _) = timed_batch(&mut session, &latency_batch(&workspace, 1)).await;
    assert_measured(&miss, 1);
    assert!(!miss.imports[0].cache_hit, "{:?}", miss.imports[0]);

    let mut hits = Vec::with_capacity(RECORDED_RUNS);
    for run in 0..(WARMUP_RUNS + RECORDED_RUNS) {
        let request_id = 2 + run as u64;
        let (hit, elapsed) =
            timed_batch(&mut session, &latency_batch(&workspace, request_id)).await;

        assert_measured(&hit, 1);
        assert!(
            hit.imports[0].cache_hit,
            "an identical repeated request must be served from the cache: {:?}",
            hit.imports[0],
        );

        if run >= WARMUP_RUNS {
            hits.push(elapsed);
        }
    }
    let hits = percentiles_of(&mut hits);

    // NFR-004 measures idle RSS "with the cache populated", which the requests above did.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle_rss = working_set_bytes(session.process_id());

    eprintln!(
        "shipped daemon cache hit ({RECORDED_RUNS} round trips): p50 {:?}, p95 {:?}, max {:?}\n\
         shipped daemon idle RSS (cache populated): {} MB",
        hits.p50,
        hits.p95,
        hits.max,
        idle_rss / (1024 * 1024),
    );

    assert!(
        hits.p95.as_millis() <= 50,
        "cache-hit response p95 exceeded the 50 ms gate (NFR-002, Critical, unscaled): {}ms",
        hits.p95.as_millis()
    );
    assert!(
        idle_rss < 100 * 1024 * 1024,
        "idle RSS exceeded the 100 MB gate (NFR-004): {idle_rss} bytes"
    );
}

// NFR-004 (High, §10.6): a 20-import active batch stays below 400 MB peak RSS — in the DAEMON.
//
// The batch is sent as one `BatchRequest` to the shipped binary, so the concurrency, the engine
// permits and the allocator are the shipped ones. It matches the spec's comparison set:
// independent packages, shared transitive dependencies, a CJS package, and repeated different
// exports from single packages.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn shipped_daemon_twenty_import_batch_peak_rss_stays_under_release_threshold() {
    const BATCH: &[(&str, &str)] = &[
        ("css-tree", "parse"),
        ("css-tree", "generate"),
        ("css-tree", "walk"),
        ("date-fns", "format"),
        ("date-fns", "addDays"),
        ("date-fns", "parseISO"),
        ("date-fns", "subDays"),
        ("lodash-es", "debounce"),
        ("lodash-es", "throttle"),
        ("lodash-es", "cloneDeep"),
        ("lodash-es", "merge"),
        ("lodash", "debounce"),
        ("zod", "z"),
        ("zod", "ZodError"),
        ("react", "useState"),
        ("react", "useEffect"),
        ("react", "useMemo"),
        ("uuid", "v4"),
        ("uuid", "v1"),
        ("uuid", "validate"),
    ];

    let workspace = common::engine_fixtures::fixtures_workspace();
    let mut session = start_daemon(&workspace).await;

    let imports = BATCH
        .iter()
        .map(|(package, export)| named_import(&workspace, package, export))
        .collect::<Vec<_>>();
    let (response, elapsed) = timed_batch(&mut session, &batch_of(&workspace, 1, imports)).await;

    assert_measured(&response, BATCH.len());
    // Read before the `Drop` kills the daemon: a dead process has no working set to report.
    let peak = peak_working_set_bytes(session.process_id());

    eprintln!(
        "shipped daemon 20-import batch: {elapsed:?} wall clock, peak RSS {} MB",
        peak / (1024 * 1024)
    );
    assert!(
        peak < 400 * 1024 * 1024,
        "20-import batch peak RSS exceeded the 400 MB gate (NFR-004): {peak} bytes"
    );
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

// DIAGNOSTIC, not the NFR-003 gate. `RolldownEngine::bundle` is ONE STEP of a cold import — the
// cold gate above measures all of them, on the same package and export, through the shipped daemon.
// Subtract the two and you get the cost of everything that is not the engine (resolution, the
// second comparison build, minification, compression, the cache write); that attribution is the
// only reason this row still exists, and it is what tells you where a red cold gate regressed.
//
// The 500 ms assertion is kept because it is a genuine necessary condition — one step of the cold
// path cannot exceed the budget for all of it — but it is subsumed by the gate above and must never
// again be mistaken for it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn engine_only_bundle_p95_diagnostic_stays_under_release_threshold() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let version = common::pipeline_fixtures::installed_version(&workspace, LATENCY_PACKAGE);
    let resolve = || {
        common::engine_fixtures::resolve_fixture_entry(
            &workspace,
            LATENCY_PACKAGE,
            &version,
            LATENCY_EXPORT,
        )
    };

    for _ in 0..WARMUP_RUNS {
        bundle_once(resolve()).await;
    }
    let mut durations = Vec::with_capacity(RECORDED_RUNS);
    let mut raw_bytes = 0usize;
    for _ in 0..RECORDED_RUNS {
        let entry = resolve();
        let start = Instant::now();
        raw_bytes = bundle_once(entry).await;
        durations.push(start.elapsed());
    }
    let engine = percentiles_of(&mut durations);
    eprintln!(
        "DIAGNOSTIC — engine build only, {LATENCY_PACKAGE}/{LATENCY_EXPORT} ({RECORDED_RUNS} runs): \
         p50 {:?}, p95 {:?}, max {:?} ({raw_bytes} raw bytes). This is one step of the cold import \
         gated above, not the cold import.",
        engine.p50, engine.p95, engine.max,
    );

    assert!(
        engine.p95.as_millis() <= threshold_ms(500),
        "engine-only bundle p95 exceeded the 500 ms budget for a whole cold import: {}ms",
        engine.p95.as_millis()
    );
}
