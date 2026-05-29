use import_lens_daemon::cache::memory::ImportCache;
use import_lens_daemon::ipc::protocol::ImportResult;

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
        error: None,
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
