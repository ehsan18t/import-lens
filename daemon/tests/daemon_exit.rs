//! The shipped daemon process ends when its connection does, whatever its handlers are still doing.

use futures_util::SinkExt;
use import_lens_daemon::ipc::{
    codec::{message_frame_codec, payload_bytes},
    protocol::{AnalyzeSpecifiersRequest, HelloMessage, PROTOCOL_VERSION},
};
use std::{
    fmt::Write as _,
    fs,
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};
use tokio_util::codec::Framed;

mod common;

#[cfg(windows)]
type DaemonStream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(not(windows))]
type DaemonStream = tokio::net::UnixStream;

/// The connection's own teardown waits at most 2s for its tasks (`TASK_JOIN_TIMEOUT`), then
/// flushes. Everything past that is the process failing to exit.
const EXIT_BOUND: Duration = Duration::from_secs(8);

/// Enough cold packages that draining them through the two engine permits takes far longer than
/// `EXIT_BOUND`, so an exit that waits for the drain cannot pass.
const SLOW_PACKAGES: usize = 12;

fn endpoint_name() -> String {
    let unique = format!(
        "il-exit-{}-{}",
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

/// Packages with a large entry, so each cold build spends real time in parse, minify and compress.
fn write_slow_packages(workspace: &Path) -> Vec<String> {
    let mut source = String::new();
    for index in 0..40_000 {
        let _ = writeln!(
            source,
            "export function f{index}(a, b) {{ return a * {index} + b - '{index}'.length; }}"
        );
    }

    (0..SLOW_PACKAGES)
        .map(|index| {
            let name = format!("slow-lib-{index}");
            let root = workspace.join("node_modules").join(&name);
            fs::create_dir_all(&root).expect("package root should be created");
            fs::write(
                root.join("package.json"),
                r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
            )
            .expect("manifest should be written");
            fs::write(root.join("index.js"), format!("// {name}\n{source}"))
                .expect("entry should be written");
            name
        })
        .collect()
}

fn wait_for_exit(child: &mut Child, bound: Duration) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < bound {
        if child.try_wait().expect("child status").is_some() {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The client vanishing (an extension host crash, a killed CLI) is EOF to the daemon. A blocking
/// handler draining engine builds cannot be cancelled, and the process must not live on until it
/// finishes: it would hold the project's cache shards open against the next daemon.
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_exits_promptly_when_its_client_vanishes_mid_drain() {
    let workspace = common::temp_workspace("import-lens-exit");
    let storage = common::temp_workspace("import-lens-exit-storage");
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
    let specifiers = write_slow_packages(&workspace);
    let endpoint = endpoint_name();

    let mut child = Command::new(env!("CARGO_BIN_EXE_import-lens-daemon"))
        .args(["--pipe", &endpoint, "--storage", &storage.to_string_lossy()])
        .spawn()
        .expect("the shipped daemon binary should start");
    let mut framed = Framed::new(connect(&endpoint).await, message_frame_codec());

    let workspace_root = workspace.to_string_lossy().into_owned();
    framed
        .send(
            payload_bytes(&HelloMessage {
                message_type: "hello".to_owned(),
                version: PROTOCOL_VERSION,
                workspace_root: workspace_root.clone(),
                storage_path: storage.to_string_lossy().into_owned(),
                enable_disk_cache: true,
                cache_max_size_mb: 64,
                registry_cache_max_size_mb: 8,
                log_level: "error".to_owned(),
            })
            .expect("hello should encode"),
        )
        .await
        .expect("hello should be written");
    framed
        .send(
            payload_bytes(&AnalyzeSpecifiersRequest {
                message_type: "analyze_specifiers".to_owned(),
                version: PROTOCOL_VERSION,
                request_id: 1,
                workspace_root,
                active_document_path: workspace
                    .join("src")
                    .join("index.ts")
                    .to_string_lossy()
                    .into_owned(),
                specifiers,
            })
            .expect("request should encode"),
        )
        .await
        .expect("request should be written");

    // Let the handler get into the engine before the client disappears.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(framed);

    let exited = wait_for_exit(&mut child, EXIT_BOUND);
    if exited.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_dir_all(&workspace);
    let _ = fs::remove_dir_all(&storage);

    assert!(
        exited.is_some(),
        "the daemon must exit within {EXIT_BOUND:?} of losing its client, not when its drain ends"
    );
}
