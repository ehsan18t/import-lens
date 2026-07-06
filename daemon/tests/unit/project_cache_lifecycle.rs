use super::*;
use crate::ipc::protocol::{ConfidenceLevel, ImportDiagnostic, ImportResult, ResultFreshness};
use std::{sync::Arc, time::Duration};

fn temp_storage(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "il-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn result(specifier: &str) -> ImportResult {
    ImportResult {
        specifier: specifier.to_owned(),
        raw_bytes: 10,
        minified_bytes: 8,
        gzip_bytes: 7,
        brotli_bytes: 6,
        zstd_bytes: 5,
        cache_hit: false,
        side_effects: false,
        truly_treeshakeable: true,
        is_cjs: false,
        confidence: ConfidenceLevel::High,
        confidence_reasons: vec!["test fixture confidence".to_owned()],
        error: None,
        diagnostics: vec![ImportDiagnostic {
            stage: "test".to_owned(),
            message: "cached".to_owned(),
            details: Vec::new(),
        }],
        module_breakdown: None,
        shared_bytes: None,
        freshness: ResultFreshness::fresh(),
        internal_contributions: Vec::new(),
    }
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
