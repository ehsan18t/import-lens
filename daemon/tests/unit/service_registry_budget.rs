use super::ImportLensService;

#[test]
fn service_stores_registry_cache_budget_from_policy() {
    let service = ImportLensService::new_with_cache_policy(None, false, 512, 7);

    assert_eq!(
        service.registry_cache_max_size_bytes,
        7 * 1024 * 1024,
        "registryCacheMaxSizeMB should be converted to bytes and stored for maintenance"
    );
}
