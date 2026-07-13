use super::{ConnectionLifecycles, close_connection};
use crate::{
    ipc::protocol::{
        BatchRequest, CacheStatusRequest, ImportKind, ImportRequest, ImportRuntime,
        PROTOCOL_VERSION,
    },
    prefetch::Prefetcher,
    service::ImportLensService,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "import-lens-teardown-{name}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(path.join("src")).expect("temp directory should be created");
    path
}

fn write_tiny_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("tiny-teardown-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
}

fn batch(workspace: &Path) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id: 1,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "tiny-teardown-lib".to_owned(),
            package_name: "tiny-teardown-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    }
}

/// Entries the project's shard holds ON DISK. Read through the same rollup the cache manager uses.
fn persisted_entries(service: &ImportLensService, workspace: &Path) -> u64 {
    service
        .cache_status(CacheStatusRequest {
            message_type: "cache_status".to_owned(),
            version: PROTOCOL_VERSION,
            request_id: 1,
            workspace_root: Some(workspace.to_string_lossy().to_string()),
        })
        .current_project
        .map_or(0, |shard| shard.entry_count)
}

/// The connection ends and the cache is flushed — on EVERY path, not only the client's orderly
/// `shutdown` (SRS FR-004c).
///
/// The extension host can die without ever sending `shutdown`; the daemon then reads EOF. That path
/// cancelled its background work and joined it and returned, and everything the session had measured
/// but not yet committed stayed in memory. `Drop` covers the entries merely QUEUED for the batched
/// commit — which is why this test does not use one: it forces an entry down the path `Drop` does
/// NOT cover (a disk insert that failed, so the key is marked dirty and only a `flush_to_disk`
/// replays it), which is precisely the class of loss the missing flush produced.
#[tokio::test]
async fn closing_a_connection_flushes_what_the_session_measured() {
    let key_probe_storage = temp_dir("probe");
    let storage = temp_dir("storage");
    let workspace = temp_dir("workspace");
    write_tiny_package(&workspace);

    // The cache key is derived from the package, not from the storage: measure the import once
    // against a throwaway shard to learn the key this workspace's import will be cached under.
    let probe =
        ImportLensService::new_with_cache_policy(Some(key_probe_storage.clone()), true, 512, 32);
    probe.handle_batch(batch(&workspace));
    let keys = probe.recent_cache_keys(&workspace, 8);
    let key = keys
        .first()
        .expect("the measured import should be cached under a key")
        .clone();
    drop(probe);

    // The connection's service. Its ONE disk insert fails, so the entry lives in memory and in the
    // dirty set — measured, and not yet durable.
    let service = ImportLensService::new_with_cache_policy(Some(storage.clone()), true, 512, 32);
    crate::cache::disk::test_support::fail_inserts_for_keys([key.clone()]);
    service.handle_batch(batch(&workspace));
    assert_eq!(
        persisted_entries(&service, &workspace),
        0,
        "the setup must leave the measured import undurable, or this test proves nothing"
    );

    close_connection(
        &service,
        &Prefetcher::new(),
        &ConnectionLifecycles::new(),
        &mut Vec::new(),
        &mut None,
    )
    .await;

    assert_eq!(
        persisted_entries(&service, &workspace),
        1,
        "closing the connection must flush the cache: an import the session measured and the \
         teardown dropped is rebuilt from scratch next session"
    );

    crate::cache::disk::test_support::clear_failures_for_token(&key);
    drop(service);
    fs::remove_dir_all(&key_probe_storage).ok();
    fs::remove_dir_all(&storage).ok();
    fs::remove_dir_all(&workspace).ok();
}
