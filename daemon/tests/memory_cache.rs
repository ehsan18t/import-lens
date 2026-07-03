use import_lens_daemon::cache::memory::ImportCache;
use import_lens_daemon::ipc::protocol::{ConfidenceLevel, ImportResult};

fn result(specifier: &str, cache_hit: bool) -> ImportResult {
    ImportResult {
        specifier: specifier.to_owned(),
        raw_bytes: 10,
        minified_bytes: 8,
        gzip_bytes: 7,
        brotli_bytes: 6,
        zstd_bytes: 5,
        cache_hit,
        side_effects: false,
        truly_treeshakeable: true,
        is_cjs: false,
        confidence: ConfidenceLevel::High,
        confidence_reasons: vec!["test fixture confidence".to_owned()],
        error: None,
        diagnostics: Vec::new(),
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
    }
}

#[test]
fn import_cache_returns_cache_hit_clone_without_mutating_stored_value() {
    let cache = ImportCache::default();
    cache.insert("react@18.3.1::default".to_owned(), result("react", false));

    let first = cache
        .get("react@18.3.1::default")
        .expect("cache entry should exist");
    let second = cache
        .get("react@18.3.1::default")
        .expect("cache entry should still exist");

    assert!(first.cache_hit);
    assert!(second.cache_hit);
}

#[test]
fn import_cache_does_not_track_recency_touches_when_disk_cache_is_disabled() {
    let cache = ImportCache::new(None, false);
    cache.insert("react@18.3.1::default".to_owned(), result("react", false));

    assert!(
        cache
            .get("react@18.3.1::default")
            .expect("cache entry should exist")
            .cache_hit
    );
    assert_eq!(cache.pending_recency_touch_count(), 0);
}

#[test]
fn import_cache_invalidates_package_prefixes() {
    let cache = ImportCache::default();
    cache.insert("react@18.3.1::default".to_owned(), result("react", false));
    cache.insert(
        "lodash-es@4.17.21::debounce".to_owned(),
        result("lodash-es", false),
    );

    cache.invalidate_package("react");

    assert!(cache.get("react@18.3.1::default").is_none());
    assert!(cache.get("lodash-es@4.17.21::debounce").is_some());
}

#[test]
fn import_cache_invalidates_subpath_entries() {
    let cache = ImportCache::default();
    cache.insert("svelte@5.0.0::*".to_owned(), result("svelte", false));
    cache.insert(
        "svelte/transition@5.0.0::fade".to_owned(),
        result("svelte/transition", false),
    );
    cache.insert(
        "svelte/store@5.0.0::writable".to_owned(),
        result("svelte/store", false),
    );
    cache.insert(
        "lodash-es@4.17.21::debounce".to_owned(),
        result("lodash-es", false),
    );

    cache.invalidate_package("svelte");

    assert!(cache.get("svelte@5.0.0::*").is_none());
    assert!(cache.get("svelte/transition@5.0.0::fade").is_none());
    assert!(cache.get("svelte/store@5.0.0::writable").is_none());
    assert!(cache.get("lodash-es@4.17.21::debounce").is_some());
}

#[test]
fn cache_hit_skips_fingerprint_restat_until_generation_bumps() {
    use import_lens_daemon::cache::key::fingerprints_for_paths;
    use import_lens_daemon::cache::memory::bump_cache_generation;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("il-memgen-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dep = dir.join("dep.js");
    std::fs::write(&dep, "export const x = 1;").expect("dep file");
    let fp = fingerprints_for_paths(vec![dep.clone()]);

    let cache = ImportCache::new(None, false);
    cache.insert_with_fingerprints("v3:aa".to_owned(), result("dep", false), fp);

    // Delete the dependency out of band. Within the same generation and TTL the
    // re-stat is skipped, so the (now stale) entry still serves.
    std::fs::remove_file(&dep).expect("delete dep");
    assert!(
        cache.get("v3:aa").is_some(),
        "should serve without re-stat inside the same generation"
    );

    // A generation bump forces a re-verify, which observes the missing file.
    bump_cache_generation();
    assert!(
        cache.get("v3:aa").is_none(),
        "generation bump should force re-verify and evict the stale entry"
    );

    std::fs::remove_dir_all(dir).expect("cleanup");
}
