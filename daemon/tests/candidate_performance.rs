//! The §10.6 runtime performance and memory gates, over the pinned REAL packages. Release-only and
//! explicitly ignored, mirroring daemon/tests/performance.rs and its IMPORT_LENS_PERF_MULTIPLIER
//! tolerance:
//!
//! ```text
//! node scripts/prepare-candidate-fixtures.mjs
//! # set IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints, then:
//! cargo test -p import-lens-daemon --release --locked \
//!     --test candidate_performance -- \
//!     --ignored --nocapture
//! ```
//!
//! **Until 2026-07-14 nothing invoked this file.** It is `#[ignore]`d and needs installed fixtures,
//! and no workflow step and no package.json script named it — while `pnpm test:performance`, which
//! CI *does* call, runs `daemon/tests/performance.rs`: the legacy suite over synthetic fixtures, a
//! different file. So a gate appeared to run and did not, and the suite written to protect the
//! engine had never executed once. `validate.yml` runs it now, on the same installed fixtures as
//! `candidate_packages`, on every pull request.
//!
//! An earlier header said startup, idle RSS and the cache-hit path were "governed by the existing
//! release baselines" because the candidate engine "is not compiled into the shipped binary". That
//! stopped being true at the Phase 3 cutover — Rolldown *is* the shipped engine — and those three
//! gates then belonged to nobody. They are measured here, against the shipped daemon binary over
//! real packages, because that is the only process whose startup and idle RSS mean anything: a
//! cargo test binary shares one process across every test in the file, so an RSS reading taken
//! inside it describes the harness, not the daemon.
//!
//! Latency gates carry the file's existing IMPORT_LENS_PERF_MULTIPLIER tolerance (default 6) for
//! shared CI hardware; the memory gates are absolute. Set IMPORT_LENS_PERF_MULTIPLIER=1 to hold a
//! run to the literal §10.6 numbers.

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, ImportRuntime, RolldownEngine,
};
use import_lens_daemon::ipc::codec::{FrameDecoder, decode_payload, encode_frame};
use import_lens_daemon::ipc::protocol::{
    BatchRequest, BatchResponse, HelloMessage, ImportKind, ImportRequest, PROTOCOL_VERSION,
};
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

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

/// Current (not peak) working set of ANOTHER process — the spawned daemon. Idle RSS is a claim
/// about the daemon, so it has to be read from the daemon.
#[cfg(windows)]
fn working_set_bytes(process_id: u32) -> u64 {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {process_id}).WorkingSet64"),
        ])
        .output()
        .expect("powershell should be available for the idle-RSS probe");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("WorkingSet64 should be numeric")
}

#[cfg(not(windows))]
fn working_set_bytes(process_id: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{process_id}/status"))
        .expect("/proc/<pid>/status should be readable");
    let kilobytes: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .expect("VmRSS should be present and numeric");
    kilobytes * 1024
}

/// Kills the daemon however this test ends. A panicking gate must not leave a daemon behind
/// holding the pipe.
struct DaemonProcess(std::process::Child);

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
async fn connect(endpoint: &str) -> tokio::net::windows::named_pipe::NamedPipeClient {
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
async fn connect(endpoint: &str) -> tokio::net::UnixStream {
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

fn fixture_batch(workspace: &std::path::Path, request_id: u64) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().into_owned(),
        active_document_path: workspace
            .join("src")
            .join("app.ts")
            .to_string_lossy()
            .into_owned(),
        imports: vec![ImportRequest {
            specifier: "date-fns".to_owned(),
            package_name: "date-fns".to_owned(),
            version: common::pipeline_fixtures::installed_version(workspace, "date-fns"),
            named: vec!["format".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::default(),
        }],
        streaming: false,
    }
}

// §10.6 hard gates: daemon startup < 500 ms, cache-hit response < 50 ms, idle RSS < 100 MB — the
// three that had no owner. They are measured against the SHIPPED BINARY over a real package,
// through the real IPC transport, because that is what each of them is a claim about: an in-process
// `ImportLensService` is not a startup, and the RSS of a test binary that has just bundled twenty
// packages is not an idle daemon.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn shipped_daemon_startup_cache_hit_and_idle_rss_stay_under_release_thresholds() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let storage = common::temp_workspace("import-lens-candidate-storage");
    let endpoint = endpoint_name();

    let launched = Instant::now();
    let daemon = DaemonProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_import-lens-daemon"))
            .args([
                "--pipe",
                &endpoint,
                "--workspace",
                &workspace.to_string_lossy(),
                "--storage",
                &storage.to_string_lossy(),
            ])
            .spawn()
            .expect("the shipped daemon binary should start"),
    );
    let process_id = daemon.0.id();

    let mut stream = connect(&endpoint).await;
    let startup = launched.elapsed();

    // The shipped configuration: the disk cache on, in a storage directory of its own.
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

    let mut decoder = FrameDecoder::default();
    let miss_start = Instant::now();
    send(&mut stream, &fixture_batch(&workspace, 1)).await;
    let miss = read_batch_response(&mut stream, &mut decoder).await;
    let miss_elapsed = miss_start.elapsed();

    let hit_start = Instant::now();
    send(&mut stream, &fixture_batch(&workspace, 2)).await;
    let hit = read_batch_response(&mut stream, &mut decoder).await;
    let hit_elapsed = hit_start.elapsed();

    // NFR-004 measures idle RSS "with the cache populated", which the two requests above did.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle_rss = working_set_bytes(process_id);
    drop(daemon);
    let _ = std::fs::remove_dir_all(&storage);

    eprintln!(
        "shipped daemon: startup {startup:?}, cold miss {miss_elapsed:?}, cache hit \
         {hit_elapsed:?}, idle RSS {} MB",
        idle_rss / (1024 * 1024)
    );

    // A gate that measured a FAILED analysis would be measuring the error path's speed, and an
    // Unmeasured result is served without ever entering the engine on the second request.
    assert!(
        miss.imports[0].sizes().is_some(),
        "the cold request must be MEASURED for these numbers to mean anything: {:?}",
        miss.imports[0],
    );
    assert!(!miss.imports[0].cache_hit, "{:?}", miss.imports[0]);
    assert!(
        hit.imports[0].cache_hit,
        "the second identical request must be served from the cache: {:?}",
        hit.imports[0],
    );

    assert!(
        startup.as_millis() <= threshold_ms(500),
        "daemon startup exceeded the 500 ms gate: {}ms",
        startup.as_millis()
    );
    assert!(
        hit_elapsed.as_millis() <= threshold_ms(50),
        "cache-hit response exceeded the 50 ms gate: {}ms",
        hit_elapsed.as_millis()
    );
    assert!(
        idle_rss < 100 * 1024 * 1024,
        "idle RSS exceeded the 100 MB gate: {idle_rss} bytes"
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
