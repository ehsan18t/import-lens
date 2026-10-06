//! Cache invalidation rewrites every shard on disk; the connection loop must keep writing frames
//! while it runs, and a request read after it must still see its effect.

use futures_util::{SinkExt, StreamExt};
use import_lens_daemon::{
    cache::project::ProjectCacheRegistry,
    ipc::{
        codec::{decode_payload, message_frame_codec, payload_bytes},
        protocol::{
            ConfidenceLevel, EnumerateExportsRequest, EnumerateExportsResponse, HelloMessage,
            ImportResult, MeasuredSizes, NodeModulesChangedMessage, PROTOCOL_VERSION,
            ShutdownMessage,
        },
        server::handle_connection,
    },
    prefetch::Prefetcher,
    service::ImportLensService,
};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{DuplexStream, duplex};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

mod common;

/// Enough on-disk shards that rewriting them all takes far longer than one small request.
const SHARDS: usize = 40;

fn write_export_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("exports-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "export const alpha = 1;\nexport const beta = 2;\n",
    )
    .expect("entry should be written");
}

/// One shard per fake project root, each holding an entry, so the invalidation opens, scans and
/// commits every one of them.
fn seed_shards(storage: &Path) {
    let registry = ProjectCacheRegistry::new(Some(storage.to_path_buf()), true, 512);
    for index in 0..SHARDS {
        let mut result = ImportResult::measured(
            "exports-lib",
            MeasuredSizes {
                raw_bytes: 10,
                minified_bytes: 8,
                gzip_bytes: 7,
                brotli_bytes: 6,
                zstd_bytes: 5,
            },
        );
        result.confidence = ConfidenceLevel::High;
        registry
            .cache_for_root(&storage.join(format!("project-{index}")))
            .insert(format!("v4:seed-{index}"), result);
    }
    registry.flush_to_disk().expect("seed shards should flush");
}

fn enumerate(workspace: &Path, request_id: u64) -> EnumerateExportsRequest {
    EnumerateExportsRequest {
        message_type: "enumerate_exports".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().into_owned(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .into_owned(),
        specifier: "exports-lib".to_owned(),
        package_name: "exports-lib".to_owned(),
        package_version: "1.0.0".to_owned(),
        cursor_offset: None,
    }
}

async fn send<T: serde::Serialize>(
    framed: &mut Framed<DuplexStream, LengthDelimitedCodec>,
    message: &T,
) {
    framed
        .send(payload_bytes(message).expect("client frame should encode"))
        .await
        .expect("client frame should be written");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_is_written_while_an_invalidation_rewrites_every_shard() {
    let workspace = common::temp_workspace("import-lens-invalidation");
    let storage = common::temp_workspace("import-lens-invalidation-storage");
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
    write_export_package(&workspace);
    seed_shards(&storage);

    let (client, server_stream) = duplex(1024 * 1024);
    let server = tokio::spawn(async move {
        handle_connection(
            server_stream,
            None,
            Arc::new(ImportLensService::new(None, false)),
            Prefetcher::new(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    let mut framed = Framed::new(client, message_frame_codec());
    send(
        &mut framed,
        &HelloMessage {
            message_type: "hello".to_owned(),
            version: PROTOCOL_VERSION,
            workspace_root: workspace.to_string_lossy().into_owned(),
            storage_path: storage.to_string_lossy().into_owned(),
            enable_disk_cache: true,
            cache_max_size_mb: 512,
            registry_cache_max_size_mb: 8,
            log_level: "error".to_owned(),
        },
    )
    .await;

    // Hello reads every shard to seed the recency clock; let it finish before the clock starts.
    send(&mut framed, &enumerate(&workspace, 0)).await;
    tokio::time::timeout(Duration::from_secs(60), framed.next())
        .await
        .expect("the warm-up response should arrive")
        .expect("the connection should stay open")
        .expect("the frame should be readable");

    let started = Instant::now();
    send(&mut framed, &enumerate(&workspace, 1)).await;
    send(
        &mut framed,
        &NodeModulesChangedMessage {
            message_type: "node_modules_changed".to_owned(),
            package_json_paths: vec![
                workspace
                    .join("node_modules")
                    .join("exports-lib")
                    .join("package.json")
                    .to_string_lossy()
                    .into_owned(),
            ],
            tsconfig_paths: Vec::new(),
        },
    )
    .await;
    send(&mut framed, &enumerate(&workspace, 2)).await;

    let mut arrivals = HashMap::new();
    while arrivals.len() < 2 {
        let frame = tokio::time::timeout(Duration::from_secs(60), framed.next())
            .await
            .expect("both responses should arrive")
            .expect("the connection should stay open")
            .expect("the frame should be readable");
        let response: EnumerateExportsResponse =
            decode_payload(&frame).expect("the response should decode");
        assert_eq!(response.error, None, "{response:?}");
        arrivals.insert(response.request_id, started.elapsed());
    }

    send(
        &mut framed,
        &ShutdownMessage {
            message_type: "shutdown".to_owned(),
        },
    )
    .await;
    server
        .await
        .expect("server task should join")
        .expect("server should exit cleanly");
    let _ = fs::remove_dir_all(&workspace);
    let _ = fs::remove_dir_all(&storage);

    // The request read after the invalidation waits for it; the one read before must not.
    let (before, after) = (arrivals[&1], arrivals[&2]);
    assert!(
        before * 2 < after,
        "a response queued before the invalidation must not wait for it: \
         before={before:?} after={after:?}"
    );
}
