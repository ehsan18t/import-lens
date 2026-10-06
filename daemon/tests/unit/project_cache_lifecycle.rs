use super::*;
use crate::ipc::protocol::{ConfidenceLevel, ImportDiagnostic, ImportResult, MeasuredSizes};
use std::{sync::Arc, time::Duration};

fn temp_storage(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "il-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn result(specifier: &str) -> ImportResult {
    let mut result = ImportResult::measured(
        specifier,
        MeasuredSizes {
            raw_bytes: 10,
            minified_bytes: 8,
            gzip_bytes: 7,
            brotli_bytes: 6,
            zstd_bytes: 5,
        },
    );
    result.truly_treeshakeable = true;
    result.confidence = ConfidenceLevel::High;
    result.confidence_reasons = vec!["test fixture confidence".to_owned()];
    result.diagnostics = vec![ImportDiagnostic {
        // A real informational stage. A fabricated one ("test") is now REFUSED by every durable
        // store — an unclassified stage is not durable (`pipeline::stage`) — so a fixture that used
        // one was building a result the cache correctly declines to keep.
        stage: crate::engine::diagnostic_stage::EXTERNAL.to_owned(),
        message: "cached".to_owned(),
        details: Vec::new(),
    }];
    result
}

#[test]
fn registry_flush_attempts_every_loaded_shard_after_one_shard_fails() {
    let storage = temp_storage("rb10-registry-flush");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
    let root_a = storage.join("workspace-a");
    let root_b = storage.join("workspace-b");
    let cache_a = registry.cache_for_root(&root_a);
    let cache_b = registry.cache_for_root(&root_b);
    let token = crate::cache::disk::test_support::unique_failure_token("rb10-registry-flush");
    let key_a = format!("v4:{token}:dirty-a");
    let key_b = format!("v4:{token}:dirty-b");

    crate::cache::disk::test_support::fail_inserts_for_keys([key_a.clone(), key_b.clone()]);
    cache_a.insert(key_a.clone(), result("a"));
    cache_b.insert(key_b.clone(), result("b"));

    crate::cache::disk::test_support::clear_insert_attempts_for_token(&token);
    crate::cache::disk::test_support::fail_inserts_for_keys([key_a.clone(), key_b.clone()]);

    let error = registry
        .flush_to_disk()
        .expect_err("loaded shard flush errors should be reported after all shards are tried");
    assert!(
        error.contains(&key_a) && error.contains(&key_b),
        "aggregate error should include both shard failures: {error}"
    );

    let attempts = crate::cache::disk::test_support::take_insert_attempts_for_token(&token)
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    assert!(
        attempts.contains(&key_a) && attempts.contains(&key_b),
        "registry flush should try both loaded shards even when one fails"
    );

    drop(cache_a);
    drop(cache_b);
    drop(registry);
    crate::cache::disk::test_support::clear_failures_for_token(&token);
    std::fs::remove_dir_all(storage).ok();
}

#[test]
fn remove_shard_by_id_waits_for_the_shard_load_lock() {
    let storage = temp_storage("rb8-remove-load-lock");
    let registry = Arc::new(ProjectCacheRegistry::new(Some(storage.clone()), true, 512));
    let project_root = storage.join("workspace");
    let shard_id = project_cache_shard_id(&project_root);
    let load_lock = registry.load_lock_for(&shard_id);
    let load_guard = load_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let remover = Arc::clone(&registry);
    let remove_id = shard_id.clone();

    let handle = std::thread::spawn(move || {
        started_tx.send(()).expect("signal remover started");
        let result = remover.remove_shard_by_id(&remove_id);
        done_tx.send(result).expect("send removal result");
    });

    started_rx.recv().expect("remover should start");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "remove_shard_by_id must wait behind an in-flight cold load for the same shard"
    );

    drop(load_guard);
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("removal should complete after the load lock releases");
    handle.join().expect("remover thread should not panic");

    drop(registry);
    std::fs::remove_dir_all(storage).ok();
}

/// A shard whose disk cannot be opened (another daemon holds the file lock, the directory is
/// read-only, a maintenance pass holds it) must keep one memory layer across requests instead of
/// a fresh, empty one per request, and must still regain persistence once the disk is free.
#[test]
fn a_shard_without_its_disk_keeps_its_memory_and_retries_the_open() {
    let storage = temp_storage("disk-retry");
    let root = storage.join("app");
    std::fs::create_dir_all(&root).expect("project root");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
    let shard_id = project_cache_shard_id(&root);
    let shard_dir = storage.join(&shard_id);
    std::fs::create_dir_all(&shard_dir).expect("shard dir");
    let held = redb::Database::create(shard_dir.join(SHARD_DB_FILE_NAME)).expect("hold the db");

    let degraded = registry.cache_for_root(&root);
    assert!(!degraded.disk_available());
    degraded.insert("react@18.3.1::default".to_owned(), result("react"));

    let next_request = registry.cache_for_root(&root);
    assert!(
        Arc::ptr_eq(&degraded, &next_request),
        "every request must share the degraded shard's memory layer"
    );
    assert!(next_request.get("react@18.3.1::default").is_some());

    drop(held);
    assert!(
        !registry.cache_for_root(&root).disk_available(),
        "the retry waits for its backoff instead of reopening on every request"
    );

    // Let the backoff elapse.
    if let Some(shard) = registry.loaded.lock().unwrap().get_mut(&shard_id) {
        shard.disk_retry = Some(DiskRetry::starting_at(0));
    }
    let healed = registry.cache_for_root(&root);
    assert!(Arc::ptr_eq(&degraded, &healed), "the shard heals in place");
    assert!(
        healed.disk_available(),
        "a due retry reopens the freed disk"
    );
    assert!(
        shard_dir.join(SHARD_METADATA_FILE_NAME).exists(),
        "a healed shard is listed again"
    );
    assert!(healed.get("react@18.3.1::default").is_some());

    // What the shard measured while its disk was missing is persisted once the disk is back.
    drop((degraded, next_request, healed, registry));
    let next_session = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
    assert!(
        next_session
            .cache_for_root(&root)
            .get_for_prewarm("react@18.3.1::default")
            .is_some(),
        "an entry computed without the disk is written when the disk reattaches"
    );

    drop(next_session);
    std::fs::remove_dir_all(storage).ok();
}
