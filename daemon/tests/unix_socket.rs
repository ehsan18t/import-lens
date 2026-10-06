#![cfg(unix)]

use import_lens_daemon::ipc::server::run_server;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::net::UnixStream;

fn socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("il-{tag}-{}.sock", std::process::id()))
}

async fn connect(path: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return stream,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("the daemon socket never accepted a connection: {error}"),
        }
    }
}

/// A killed daemon runs no cleanup at all, so the only unlink that cannot be skipped is the one
/// done while the process is still healthy: right after its single client connects.
#[tokio::test]
async fn the_socket_file_is_gone_once_the_client_is_connected() {
    let path = socket_path("unlink");
    let pipe = path.to_string_lossy().into_owned();

    let client = async {
        let stream = connect(&path).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::symlink_metadata(&path).is_ok() {
            assert!(
                Instant::now() < deadline,
                "the socket file must be unlinked while the connection is still open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // EOF ends the connection.
        drop(stream);
    };
    let (served, ()) = tokio::join!(run_server(&pipe, None), client);

    served.expect("the daemon should end cleanly on EOF");
    assert!(std::fs::symlink_metadata(&path).is_err());
}

#[tokio::test]
async fn a_socket_path_past_the_platform_limit_fails_at_once_and_names_its_length() {
    let path = std::env::temp_dir().join(format!("il-{}.sock", "x".repeat(120)));
    let pipe = path.to_string_lossy().into_owned();

    let error = tokio::time::timeout(Duration::from_secs(5), run_server(&pipe, None))
        .await
        .expect("the bind must fail at once, not wait for a client")
        .expect_err("a path past sun_path cannot be bound");

    assert!(
        error
            .to_string()
            .contains(&format!("({} bytes)", pipe.len())),
        "the error must carry the path length: {error}"
    );
}
