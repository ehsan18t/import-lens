//! The §10.6 runtime performance and memory gates, over the pinned REAL packages. Release-only and
//! explicitly ignored:
//!
//! ```text
//! node scripts/prepare-candidate-fixtures.mjs
//! # set IMPORT_LENS_FIXTURES_WORKSPACE to the directory it prints, then:
//! cargo test -p import-lens-daemon --release --locked \
//!     --test candidate_performance -- \
//!     --ignored --nocapture --test-threads=1
//! ```
//!
//! `validate.yml` runs it on the same installed fixtures as `candidate_packages`, on every pull
//! request.
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

use futures_util::{SinkExt, StreamExt};
use import_lens_daemon::engine::{
    AssetKind, BundleEntry, BundlePurpose, BundleRequest, ImportRuntime, RolldownEngine,
};
use import_lens_daemon::ipc::codec::{decode_payload, message_frame_codec, payload_bytes};
use import_lens_daemon::ipc::protocol::{
    AnalyzeDocumentRequest, AnalyzeDocumentResponse, CacheInvalidateAllMessage, HelloMessage,
    ImportAnalysisItem, ImportResult, PROTOCOL_VERSION, RefreshedResultsResponse,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

mod common;

/// The NFR-004 comparison set: independent packages, shared transitive dependencies, a CJS package,
/// and repeated different exports from single packages.
const TWENTY_IMPORT_BATCH: &[(&str, &str)] = &[
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

/// §10.6: "five warm-up runs followed by at least 30 recorded runs".
const WARMUP_RUNS: usize = 5;
const RECORDED_RUNS: usize = 30;
const ASSET_BINARY_BYTES: usize = 1024 * 1024;
const ASSET_BATCH_PER_KIND: usize = 4;

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
    framed: Framed<DaemonStream, LengthDelimitedCodec>,
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
        .args(["--pipe", &endpoint, "--storage", &storage.to_string_lossy()])
        .spawn()
        .expect("the shipped daemon binary should start");
    let mut framed = Framed::new(connect(&endpoint).await, message_frame_codec());
    let startup = launched.elapsed();

    send(
        &mut framed,
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
        framed,
        storage,
        startup,
    }
}

async fn send<T: serde::Serialize>(
    framed: &mut Framed<DaemonStream, LengthDelimitedCodec>,
    message: &T,
) {
    framed
        .send(payload_bytes(message).expect("client frame should encode"))
        .await
        .expect("client frame should be writable");
}

/// Every import of one analyzed document, each with the result it finally settled on.
#[derive(Debug)]
struct AnalyzedDocument {
    imports: Vec<ImportResult>,
}

/// Read until every import of the document `request_id` names has a result: the cache hits ride on
/// the response, and each miss arrives as its own `refreshed_results` push. Frames left over from an
/// earlier request (a late shared-bytes correction) are skipped.
async fn read_analyzed_document(
    framed: &mut Framed<DaemonStream, LengthDelimitedCodec>,
    request_id: u64,
) -> AnalyzedDocument {
    let mut items: Option<Vec<ImportAnalysisItem>> = None;
    loop {
        if let Some(items) = &items
            && items.iter().all(|item| item.result.is_some())
        {
            break;
        }

        let payload = tokio::time::timeout(Duration::from_secs(60), framed.next())
            .await
            .expect("daemon should answer within 60s")
            .expect("daemon closed the connection before answering")
            .expect("daemon frame should be readable");
        if let Ok(response) = decode_payload::<AnalyzeDocumentResponse>(&payload) {
            if response.request_id == request_id {
                assert_eq!(response.error, None, "{response:?}");
                items = Some(response.imports);
            }
        } else if let Ok(push) = decode_payload::<RefreshedResultsResponse>(&payload)
            && push.generation == Some(request_id)
            && let Some(items) = items.as_mut()
        {
            for (result, identity) in push.results.into_iter().zip(push.identities) {
                if let Some(item) = items.iter_mut().find(|item| {
                    item.detected.specifier == identity.specifier
                        && item.detected.import_kind == identity.import_kind
                        && item.detected.named == identity.named
                        && item.detected.runtime == identity.runtime
                }) {
                    item.result = Some(result);
                }
            }
        }
    }

    AnalyzedDocument {
        imports: items
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| item.result)
            .collect(),
    }
}

/// One document analysis, timed the way the user experiences it: from the moment the request
/// leaves to the moment every import in it has its number.
async fn timed_document(
    session: &mut DaemonSession,
    request: &AnalyzeDocumentRequest,
) -> (AnalyzedDocument, Duration) {
    let start = Instant::now();
    send(&mut session.framed, request).await;
    let analyzed = read_analyzed_document(&mut session.framed, request.request_id).await;
    (analyzed, start.elapsed())
}

/// A document importing each `(package, export)` pair with its own named import statement. Each
/// binding is aliased, so two packages exporting the same name do not collide.
fn document_of(
    workspace: &Path,
    request_id: u64,
    imports: &[(&str, &str)],
) -> AnalyzeDocumentRequest {
    let source = imports
        .iter()
        .enumerate()
        .map(|(index, (package, export))| {
            format!("import {{ {export} as binding{index} }} from '{package}';\n")
        })
        .collect::<String>();
    AnalyzeDocumentRequest {
        message_type: "analyze_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().into_owned(),
        active_document_path: workspace
            .join("src")
            .join("app.ts")
            .to_string_lossy()
            .into_owned(),
        source,
    }
}

fn latency_document(workspace: &Path, request_id: u64) -> AnalyzeDocumentRequest {
    document_of(workspace, request_id, &[(LATENCY_PACKAGE, LATENCY_EXPORT)])
}

fn asset_document(
    workspace: &Path,
    request_id: u64,
    packages: &[String],
) -> AnalyzeDocumentRequest {
    let imports = packages
        .iter()
        .map(|package| (package.as_str(), "value"))
        .collect::<Vec<_>>();
    document_of(workspace, request_id, &imports)
}

fn write_css_heavy_package(workspace: &Path, package: &str) {
    const CHILDREN: usize = 32;
    const RULES_PER_CHILD: usize = 128;
    let root = workspace.join("node_modules").join(package);
    fs::create_dir_all(root.join("styles")).expect("CSS fixture directory should be created");
    fs::write(
        root.join("package.json"),
        format!(
            r#"{{"name":"{package}","version":"1.0.0","module":"index.js","sideEffects":true}}"#
        ),
    )
    .expect("CSS fixture manifest should be written");
    fs::write(
        root.join("index.js"),
        "import './styles.css';\nexport const value = 1;\n",
    )
    .expect("CSS fixture entry should be written");
    let imports = (0..CHILDREN)
        .map(|index| format!("@import './styles/child-{index}.css';"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(root.join("styles.css"), imports).expect("CSS fixture root should be written");
    for child in 0..CHILDREN {
        let rules = (0..RULES_PER_CHILD)
            .map(|rule| {
                format!(
                    ".asset-{child}-{rule} {{ color: rgb({}, {}, {}); padding: {rule}px; }}",
                    child % 255,
                    rule % 255,
                    (child + rule) % 255
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            root.join("styles").join(format!("child-{child}.css")),
            rules,
        )
        .expect("CSS fixture child should be written");
    }
}

fn write_binary_heavy_package(workspace: &Path, package: &str) {
    let root = workspace.join("node_modules").join(package);
    fs::create_dir_all(&root).expect("binary fixture directory should be created");
    fs::write(
        root.join("package.json"),
        format!(
            r#"{{"name":"{package}","version":"1.0.0","module":"index.js","sideEffects":true}}"#
        ),
    )
    .expect("binary fixture manifest should be written");
    fs::write(
        root.join("index.js"),
        "import './font.woff2';\nimport './payload.wasm';\nexport const value = 1;\n",
    )
    .expect("binary fixture entry should be written");
    let pseudo_random_bytes = |mut state: u64| {
        (0..ASSET_BINARY_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 56) as u8
            })
            .collect::<Vec<_>>()
    };
    fs::write(
        root.join("font.woff2"),
        pseudo_random_bytes(0x4f2c_91ab_38d7_e605),
    )
    .expect("font fixture should be written");
    fs::write(
        root.join("payload.wasm"),
        pseudo_random_bytes(0x9a71_5e24_c603_b8fd),
    )
    .expect("wasm fixture should be written");
}

/// A gate that timed a FAILED analysis would be timing the error path, and an Unmeasured result
/// never enters the engine at all — it would make every gate here trivially green.
fn assert_measured(response: &AnalyzedDocument, expected: usize) {
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

fn assert_asset_breakdown(result: &ImportResult, expected: &[(AssetKind, Option<u64>)]) {
    assert_eq!(
        result.asset_breakdown.len(),
        expected.len(),
        "`{}` must expose exactly the asset kinds its fixture ships: {:?}",
        result.specifier,
        result.asset_breakdown,
    );
    for (kind, expected_raw_bytes) in expected {
        let contribution = result
            .asset_breakdown
            .iter()
            .find(|contribution| contribution.kind == *kind)
            .unwrap_or_else(|| {
                panic!(
                    "`{}` must report a {kind:?} contribution: {:?}",
                    result.specifier, result.asset_breakdown,
                )
            });
        assert!(
            contribution.raw_bytes > 0,
            "`{}` must perform real {kind:?} asset work: {contribution:?}",
            result.specifier,
        );
        if let Some(expected_raw_bytes) = expected_raw_bytes {
            assert_eq!(
                contribution.raw_bytes, *expected_raw_bytes,
                "`{}` must count the complete {kind:?} fixture",
                result.specifier,
            );
        }
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
        let (miss, elapsed) = timed_document(&mut session, &latency_document(&workspace, 1)).await;

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

    let (miss, _) = timed_document(&mut session, &latency_document(&workspace, 1)).await;
    assert_measured(&miss, 1);
    assert!(!miss.imports[0].cache_hit, "{:?}", miss.imports[0]);

    let mut hits = Vec::with_capacity(RECORDED_RUNS);
    for run in 0..(WARMUP_RUNS + RECORDED_RUNS) {
        let request_id = 2 + run as u64;
        let (hit, elapsed) =
            timed_document(&mut session, &latency_document(&workspace, request_id)).await;

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
// The imports are sent as one document analysis to the shipped binary, so the concurrency, the
// engine permits and the allocator are the shipped ones. It matches the spec's comparison set:
// independent packages, shared transitive dependencies, a CJS package, and repeated different
// exports from single packages.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn shipped_daemon_twenty_import_batch_peak_rss_stays_under_release_threshold() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let mut session = start_daemon(&workspace).await;

    let (response, elapsed) = timed_document(
        &mut session,
        &document_of(&workspace, 1, TWENTY_IMPORT_BATCH),
    )
    .await;

    assert_measured(&response, TWENTY_IMPORT_BATCH.len());
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

/// The most Retained Memory (ADR-0007) a full cache clear may leave behind.
const RETAINED_MEMORY_LIMIT_BYTES: u64 = 60 * 1024 * 1024;

// ADR-0007: Retained Memory. After a heavy session and a full cache clear, live data is near zero, so
// whatever stays resident is memory no allocation needs: freed pages a thread's heap kept, and the
// stacks of threads that only wait. It grows with thread count, not with the project, which is why
// this bound is absolute while no gate caps RSS with the cache populated.
//
// On these fixtures, 33 MB (Windows) and 38 MB (Linux) with one CPU pool and idle reclaim; without
// them, 81 MB and 84 MB. The settle wait covers the reclaim sweep, which runs two seconds after
// activity stops.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only candidate measurement; requires installed fixtures (scripts/prepare-candidate-fixtures.mjs)"]
async fn shipped_daemon_retained_memory_after_a_full_clear_stays_bounded() {
    let workspace = common::engine_fixtures::fixtures_workspace();
    let mut session = start_daemon(&workspace).await;

    for (index, chunk) in TWENTY_IMPORT_BATCH.chunks(4).enumerate() {
        let (response, _) = timed_document(
            &mut session,
            &document_of(&workspace, index as u64 + 1, chunk),
        )
        .await;
        assert_measured(&response, chunk.len());
    }
    let (response, _) = timed_document(
        &mut session,
        &document_of(&workspace, 100, TWENTY_IMPORT_BATCH),
    )
    .await;
    assert_measured(&response, TWENTY_IMPORT_BATCH.len());

    send(
        &mut session.framed,
        &CacheInvalidateAllMessage {
            message_type: "cache_invalidate_all".to_owned(),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let retained = working_set_bytes(session.process_id());

    eprintln!(
        "shipped daemon after a full cache clear: {} MB resident",
        retained / (1024 * 1024)
    );
    assert!(
        retained < RETAINED_MEMORY_LIMIT_BYTES,
        "the daemon kept {retained} bytes resident after a full cache clear (limit \
         {RETAINED_MEMORY_LIMIT_BYTES}): idle threads are holding freed memory (ADR-0007)"
    );
}

// AC-03: the post-build asset tail has its own two-wide admission and eight-second deadline. The
// ordinary real-package fixture above is JS-heavy, so it cannot detect a regression that queues
// unbounded Lightning CSS/compressor work or retains several large asset snapshots at once. These
// synthetic packages keep every request a cache miss while exercising the two expensive shapes
// independently: a broad @import tree and two emitted binary artifacts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "release-only asset-processing p95/RSS measurement"]
async fn shipped_daemon_asset_heavy_p95_and_peak_rss_stay_bounded() {
    let workspace = common::temp_workspace("import-lens-asset-performance");
    fs::create_dir_all(workspace.join("src")).expect("fixture source directory should be created");
    let runs = WARMUP_RUNS + RECORDED_RUNS;
    let css_packages = (0..runs)
        .map(|run| format!("asset-css-{run}"))
        .collect::<Vec<_>>();
    let binary_packages = (0..runs)
        .map(|run| format!("asset-binary-{run}"))
        .collect::<Vec<_>>();
    let batch_css_packages = (0..ASSET_BATCH_PER_KIND)
        .map(|index| format!("asset-css-batch-{index}"))
        .collect::<Vec<_>>();
    let batch_binary_packages = (0..ASSET_BATCH_PER_KIND)
        .map(|index| format!("asset-binary-batch-{index}"))
        .collect::<Vec<_>>();
    for package in &css_packages {
        write_css_heavy_package(&workspace, package);
    }
    for package in &batch_css_packages {
        write_css_heavy_package(&workspace, package);
    }
    for package in &binary_packages {
        write_binary_heavy_package(&workspace, package);
    }
    for package in &batch_binary_packages {
        write_binary_heavy_package(&workspace, package);
    }

    let mut session = start_daemon(&workspace).await;
    let mut css_durations = Vec::with_capacity(RECORDED_RUNS);
    for (run, package) in css_packages.iter().enumerate() {
        let request = asset_document(&workspace, run as u64 + 1, std::slice::from_ref(package));
        let (response, elapsed) = timed_document(&mut session, &request).await;
        assert_measured(&response, 1);
        assert!(!response.imports[0].cache_hit, "{response:?}");
        assert_asset_breakdown(&response.imports[0], &[(AssetKind::Css, None)]);
        if run >= WARMUP_RUNS {
            css_durations.push(elapsed);
        }
    }

    let mut binary_durations = Vec::with_capacity(RECORDED_RUNS);
    for (run, package) in binary_packages.iter().enumerate() {
        let request = asset_document(
            &workspace,
            (runs + run) as u64 + 1,
            std::slice::from_ref(package),
        );
        let (response, elapsed) = timed_document(&mut session, &request).await;
        assert_measured(&response, 1);
        assert!(!response.imports[0].cache_hit, "{response:?}");
        assert_asset_breakdown(
            &response.imports[0],
            &[
                (AssetKind::Font, Some(ASSET_BINARY_BYTES as u64)),
                (AssetKind::Wasm, Some(ASSET_BINARY_BYTES as u64)),
            ],
        );
        if run >= WARMUP_RUNS {
            binary_durations.push(elapsed);
        }
    }

    // A fresh multi-import request makes several post-build tails contend for the dedicated
    // two-wide boundary. Sequential single-import samples above establish latency, but cannot
    // expose widened admission or concurrent retention in the daemon's high-water RSS.
    let batch_packages = batch_css_packages
        .iter()
        .chain(&batch_binary_packages)
        .cloned()
        .collect::<Vec<_>>();
    let (batch_response, batch_elapsed) = timed_document(
        &mut session,
        &asset_document(&workspace, (runs * 2) as u64 + 1, &batch_packages),
    )
    .await;
    assert_measured(&batch_response, ASSET_BATCH_PER_KIND * 2);
    for result in &batch_response.imports {
        assert!(!result.cache_hit, "{result:?}");
        if result.specifier.starts_with("asset-css-batch-") {
            assert_asset_breakdown(result, &[(AssetKind::Css, None)]);
        } else if result.specifier.starts_with("asset-binary-batch-") {
            assert_asset_breakdown(
                result,
                &[
                    (AssetKind::Font, Some(ASSET_BINARY_BYTES as u64)),
                    (AssetKind::Wasm, Some(ASSET_BINARY_BYTES as u64)),
                ],
            );
        } else {
            panic!("unexpected active-batch fixture: {}", result.specifier);
        }
    }

    let css = percentiles_of(&mut css_durations);
    let binary = percentiles_of(&mut binary_durations);
    let peak = peak_working_set_bytes(session.process_id());
    eprintln!(
        "shipped daemon CSS-heavy cold asset tail ({RECORDED_RUNS} runs): p50 {:?}, p95 {:?}, \
         max {:?}\nshipped daemon binary-heavy cold asset tail ({RECORDED_RUNS} runs): p50 {:?}, \
         p95 {:?}, max {:?}\nasset-heavy {}-import contention batch: {batch_elapsed:?}\nasset-heavy \
         peak RSS: {} MB",
        css.p50,
        css.p95,
        css.max,
        binary.p50,
        binary.p95,
        binary.max,
        ASSET_BATCH_PER_KIND * 2,
        peak / (1024 * 1024),
    );

    assert!(
        css.p95.as_millis() <= threshold_ms(500),
        "CSS-heavy asset p95 exceeded the 500 ms cold-import gate: {}ms",
        css.p95.as_millis()
    );
    assert!(
        binary.p95.as_millis() <= threshold_ms(500),
        "binary-heavy asset p95 exceeded the 500 ms cold-import gate: {}ms",
        binary.p95.as_millis()
    );
    assert!(
        peak < 400 * 1024 * 1024,
        "asset-heavy peak RSS exceeded the 400 MB active-computation gate: {peak} bytes"
    );

    drop(session);
    fs::remove_dir_all(workspace).expect("asset performance workspace cleanup");
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
