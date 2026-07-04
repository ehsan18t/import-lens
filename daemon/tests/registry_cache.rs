mod common;

use import_lens_daemon::registry::{cache::RegistryMetadataCache, types::RegistryPackageMetadata};

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
