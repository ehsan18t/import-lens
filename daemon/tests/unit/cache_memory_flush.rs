use super::ImportCache;
use crate::ipc::protocol::{ConfidenceLevel, ImportDiagnostic, ImportResult, ResultFreshness};
use std::collections::HashSet;

fn result(specifier: &str) -> ImportResult {
    ImportResult {
        specifier: specifier.to_owned(),
        raw_bytes: 1,
        minified_bytes: 1,
        gzip_bytes: 1,
        brotli_bytes: 1,
        zstd_bytes: 1,
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
fn flush_to_disk_attempts_every_dirty_entry_after_one_insert_fails() {
    let dir = std::env::temp_dir().join(format!(
        "il-rb10-dirty-flush-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let cache = ImportCache::new(Some(dir.clone()), true);
    let token = crate::cache::disk::test_support::unique_failure_token("rb10-dirty-flush");
    let fail_key = format!("v4:{token}:dirty-fail");
    let good_keys = vec![
        format!("v4:{token}:dirty-good-a"),
        format!("v4:{token}:dirty-good-b"),
    ];
    let mut all_keys = vec![fail_key.clone()];
    all_keys.extend(good_keys.iter().cloned());

    crate::cache::disk::test_support::fail_inserts_for_keys(all_keys.clone());
    for key in &all_keys {
        cache.insert(key.clone(), result(key));
    }
    assert_eq!(
        cache.dirty.lock().unwrap().len(),
        all_keys.len(),
        "the setup should leave every key dirty after forced insert failures"
    );

    crate::cache::disk::test_support::clear_insert_attempts_for_token(&token);
    crate::cache::disk::test_support::fail_inserts_for_keys([fail_key.clone()]);

    let error = cache
        .flush_to_disk()
        .expect_err("one dirty replay failure should still be reported");
    assert!(
        error.contains(&fail_key),
        "the aggregate error should identify the failed dirty key: {error}"
    );

    let attempted = crate::cache::disk::test_support::take_insert_attempts_for_token(&token)
        .into_iter()
        .collect::<HashSet<_>>();
    for key in &all_keys {
        assert!(
            attempted.contains(key),
            "flush_to_disk should attempt dirty key {key} even after another replay fails"
        );
    }
    assert_eq!(
        cache.dirty.lock().unwrap().clone(),
        HashSet::from([fail_key.clone()]),
        "only the failed dirty key should remain dirty after the flush"
    );

    drop(cache);
    let reloaded = ImportCache::new(Some(dir.clone()), true);
    for key in &good_keys {
        assert!(
            reloaded.get_for_prewarm(key).is_some(),
            "successful dirty replay for {key} should survive reload"
        );
    }
    assert!(
        reloaded.get_for_prewarm(&fail_key).is_none(),
        "the failed dirty replay should not appear durable"
    );

    drop(reloaded);
    crate::cache::disk::test_support::clear_failures_for_token(&token);
    std::fs::remove_dir_all(&dir).ok();
}
