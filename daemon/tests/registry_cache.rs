mod common;

use import_lens_daemon::registry::{
    cache::RegistryMetadataCache,
    constants::{REGISTRY_CACHE_FILE_NAME, REGISTRY_RETENTION_MS},
    types::RegistryPackageMetadata,
};
use std::fs;

fn metadata(latest: &str) -> RegistryPackageMetadata {
    RegistryPackageMetadata {
        latest_version: Some(latest.to_owned()),
        latest_published_at: None,
        deprecated_versions: Vec::new(),
    }
}

#[test]
fn persist_merges_entries_written_by_another_process() {
    let dir = common::temp_workspace("import-lens-registry-merge");

    // Window A loads and holds only `react`.
    let cache_a = RegistryMetadataCache::new(dir.clone());
    cache_a
        .write_metadata("react", metadata("18.0.0"), 100)
        .expect("write react");
    cache_a.flush().expect("flush A");

    // Window B loads the same global file and caches a disjoint package after
    // A already holds its in-memory map.
    let cache_b = RegistryMetadataCache::new(dir.clone());
    cache_b
        .write_metadata("vue", metadata("3.4.0"), 200)
        .expect("write vue");
    cache_b.flush().expect("flush B");

    // A persists again. A plain full-snapshot overwrite would drop `vue`
    // (A never had it); merge-on-persist must keep it.
    cache_a
        .write_metadata("svelte", metadata("4.0.0"), 300)
        .expect("write svelte");
    cache_a.flush().expect("flush A again");

    let reloaded = RegistryMetadataCache::new(dir);
    assert!(reloaded.get("react").is_some(), "react should survive");
    assert!(
        reloaded.get("vue").is_some(),
        "vue must not be clobbered by A's snapshot"
    );
    assert!(reloaded.get("svelte").is_some(), "svelte should be written");
}

#[test]
fn schema_mismatch_wipes_on_load() {
    let dir = common::temp_workspace("import-lens-registry-schema");
    let path = dir.join(REGISTRY_CACHE_FILE_NAME);

    // Pre-envelope bare-HashMap file (the committed redesign's on-disk format).
    // It must fail to parse as the versioned envelope and be wiped, not
    // misinterpreted (the sanctioned one-time cold-cache moment, §11).
    fs::write(
        &path,
        r#"{"left-pad":{"updated_at":100,"not_found":false}}"#,
    )
    .expect("seed legacy bare-map cache file");

    let cache = RegistryMetadataCache::new(dir.clone());
    assert!(
        cache.get("left-pad").is_none(),
        "legacy bare-map format must be wiped on load (schema mismatch)"
    );

    // A current-schema-version snapshot round-trips through a fresh reconstruct.
    cache
        .write_metadata("react", metadata("18.0.0"), 200)
        .expect("write react");
    cache.flush().expect("flush");

    let reloaded = RegistryMetadataCache::new(dir);
    assert!(
        reloaded.get("react").is_some(),
        "current schema-version snapshot must round-trip"
    );
}

#[test]
fn unrecognized_schema_version_wiped_on_load() {
    let dir = common::temp_workspace("import-lens-registry-version");
    let path = dir.join(REGISTRY_CACHE_FILE_NAME);

    // A structurally valid envelope stamped with a schema version this build
    // does not understand must be wiped rather than loaded.
    fs::write(
        &path,
        r#"{"schema_version":999,"entries":{"left-pad":{"updated_at":100,"not_found":false}}}"#,
    )
    .expect("seed wrong-version envelope");

    let cache = RegistryMetadataCache::new(dir);
    assert!(
        cache.get("left-pad").is_none(),
        "an unrecognized schema_version must be wiped on load"
    );
}

#[test]
fn clear_sticks_bypassing_union() {
    let dir = common::temp_workspace("import-lens-registry-clear");

    let cache = RegistryMetadataCache::new(dir.clone());
    cache
        .write_metadata("react", metadata("18.0.0"), 100)
        .expect("write react");
    cache.flush().expect("flush");

    // clear() must persist an authoritative empty snapshot that BYPASSES the
    // persist-time union; a union-keeping write would merge the on-disk `react`
    // straight back in and resurrect it (X-14). It now reports the write (D-a).
    cache.clear().expect("clear persists the empty snapshot");

    let reloaded = RegistryMetadataCache::new(dir);
    assert!(
        reloaded.get("react").is_none(),
        "cleared entry must not resurrect through the persist-time union"
    );
}

#[test]
fn auto_retention_drops_expired_and_sticks() {
    let dir = common::temp_workspace("import-lens-registry-retention");
    // Large `now` so `now - updated_at` for the expired entry stays positive.
    let now = 1_000 * REGISTRY_RETENTION_MS;

    let cache = RegistryMetadataCache::new(dir.clone());
    cache
        .write_metadata("fresh", metadata("1.0.0"), now)
        .expect("write fresh");
    cache
        .write_metadata(
            "expired",
            metadata("1.0.0"),
            now - REGISTRY_RETENTION_MS - 1,
        )
        .expect("write expired");
    cache.flush().expect("flush");

    // Effectively-unbounded byte cap: only the 30-day retention prune acts.
    let removed = cache.run_maintenance(now, u64::MAX);
    assert_eq!(
        removed, 1,
        "exactly the one past-retention entry is dropped"
    );

    // The maintenance write is authoritative (union = false), so the deletion
    // must survive a fresh reconstruct over the same shared file rather than
    // being resurrected by a persist-time union.
    let reloaded = RegistryMetadataCache::new(dir);
    assert!(reloaded.get("fresh").is_some(), "fresh entry must survive");
    assert!(
        reloaded.get("expired").is_none(),
        "expired entry must be gone and stay gone after reconstruct"
    );
}

#[test]
fn size_cap_evicts_oldest_until_under_budget() {
    let dir = common::temp_workspace("import-lens-registry-sizecap");
    // Every entry is fresh (well within retention) so ONLY the byte cap evicts.
    // `updated_at = now - i` makes pkg-000 the newest and pkg-(count-1) the oldest.
    let now = 1_000 * REGISTRY_RETENTION_MS;
    let count = 80u64;

    let cache = RegistryMetadataCache::new(dir.clone());
    for i in 0..count {
        cache
            .write_metadata(&format!("pkg-{i:03}"), metadata("1.0.0"), now - i)
            .expect("write entry");
    }
    cache.flush().expect("flush");

    let cache_file = dir.join(REGISTRY_CACHE_FILE_NAME);
    let full_bytes = fs::metadata(&cache_file).expect("cache file").len();
    // A budget under half the full snapshot forces real eviction while leaving a
    // comfortable surviving suffix of newest entries.
    let cap = full_bytes / 3;

    let removed = cache.run_maintenance(now, cap);
    assert!(removed > 0, "size cap must evict at least one entry");

    let persisted = fs::metadata(&cache_file).expect("cache file").len();
    assert!(
        persisted <= cap,
        "persisted snapshot ({persisted} bytes) must be within the cap ({cap} bytes)"
    );

    // Eviction is oldest-`updated_at`-first: the very newest survives, the very
    // oldest is gone.
    let reloaded = RegistryMetadataCache::new(dir);
    assert!(
        reloaded.get("pkg-000").is_some(),
        "newest entry must survive eviction"
    );
    assert!(
        reloaded.get(&format!("pkg-{:03}", count - 1)).is_none(),
        "oldest-updated_at entry must be evicted first"
    );
}
