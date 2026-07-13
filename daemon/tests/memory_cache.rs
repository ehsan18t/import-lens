use import_lens_daemon::cache::memory::ImportCache;
use import_lens_daemon::ipc::protocol::{ConfidenceLevel, ImportResult, MeasuredSizes};

fn result(specifier: &str, cache_hit: bool) -> ImportResult {
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
    result.cache_hit = cache_hit;
    result.truly_treeshakeable = true;
    result.confidence = ConfidenceLevel::High;
    result.confidence_reasons = vec!["test fixture confidence".to_owned()];
    result
}

/// Reads a key's PERSISTED `last_seq` straight off disk via a standalone
/// `DiskCache` handle that is opened and dropped within this call. The
/// disk-hydration promotion tests below need this because integration tests
/// (this file) have no access to `ImportCache`'s private in-memory map — only
/// its public API and the also-public `DiskCache`.
fn disk_persisted_last_seq(shard: &std::path::Path, key: &str) -> u64 {
    use import_lens_daemon::cache::disk::DiskCache;
    use std::sync::atomic::Ordering;

    let disk = DiskCache::new(Some(shard.to_path_buf()), true);
    disk.get_with_freshness(key)
        .unwrap_or_else(|| panic!("entry for {key} should be present on disk"))
        .0
        .last_seq
        .load(Ordering::Relaxed)
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
fn import_cache_serves_memory_hits_when_disk_cache_is_disabled() {
    let cache = ImportCache::new(None, false);
    cache.insert("react@18.3.1::default".to_owned(), result("react", false));

    // Memory-only mode: recency lives in-entry (bumped on hit), and there is no
    // disk byte budget or recents queue.
    assert!(
        cache
            .get("react@18.3.1::default")
            .expect("cache entry should exist")
            .cache_hit
    );
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
fn import_cache_purge_orphan_entries_removes_uninstalled_package_entries() {
    use import_lens_daemon::cache::key::{ANALYZER_VERSION, cache_key_for_resolved_import};
    use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
    use import_lens_daemon::pipeline::resolver::resolve_package_entry;
    use std::fs;

    let workspace = std::env::temp_dir().join(format!("il-purge-orphan-{}", std::process::id()));
    let package_root = workspace.join("node_modules").join("purge-lib");
    fs::create_dir_all(&package_root).expect("package root");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js"}"#,
    )
    .expect("manifest");
    fs::write(package_root.join("index.js"), "export const value = 1;").expect("entry");

    let document = workspace.join("src").join("index.ts");
    let request = ImportRequest {
        specifier: "purge-lib".to_owned(),
        package_name: "purge-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    };
    let resolved = resolve_package_entry(&document, &request).expect("package should resolve");
    let key = cache_key_for_resolved_import(&request, &resolved);

    let cache = ImportCache::new(None, false);
    cache.insert(key.clone(), result("purge-lib", false));

    // Paths still exist -> not an orphan -> survives the purge.
    cache.purge_orphan_entries(ANALYZER_VERSION);
    assert!(cache.get(&key).is_some());

    // Uninstall the package -> its resolved paths are gone -> orphan -> purged.
    fs::remove_dir_all(&package_root).expect("uninstall package");
    cache.purge_orphan_entries(ANALYZER_VERSION);
    assert!(
        cache.get(&key).is_none(),
        "orphan entry for an uninstalled package should be purged"
    );

    fs::remove_dir_all(&workspace).ok();
}

#[test]
fn import_cache_purge_orphan_entries_drops_disk_entries_on_version_mismatch() {
    use import_lens_daemon::cache::key::{ANALYZER_VERSION, cache_key_for_resolved_import};
    use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
    use import_lens_daemon::pipeline::resolver::resolve_package_entry;
    use std::fs;

    let workspace = std::env::temp_dir().join(format!("il-disk-purge-{}", std::process::id()));
    let package_root = workspace.join("node_modules").join("disk-purge-lib");
    fs::create_dir_all(&package_root).expect("package root");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js"}"#,
    )
    .expect("manifest");
    fs::write(package_root.join("index.js"), "export const value = 1;").expect("entry");

    let document = workspace.join("src").join("index.ts");
    let request = ImportRequest {
        specifier: "disk-purge-lib".to_owned(),
        package_name: "disk-purge-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    };
    let resolved = resolve_package_entry(&document, &request).expect("package should resolve");
    let key = cache_key_for_resolved_import(&request, &resolved);

    // Disk-ENABLED so the redb scan/remove in DiskCache::purge_orphan_entries runs.
    let cache = ImportCache::new(Some(workspace.join("shard")), true);
    cache.insert(key.clone(), result("disk-purge-lib", false));
    cache.flush_to_disk().expect("flush to disk");

    // Live paths + current analyzer version -> survives the disk + memory scan.
    cache.purge_orphan_entries(ANALYZER_VERSION);
    assert!(cache.get(&key).is_some());

    // A different analyzer version marks every current entry as release-stale.
    cache.purge_orphan_entries("some-other-analyzer-version");
    assert!(
        cache.get(&key).is_none(),
        "stale-analyzer-version entry should be purged from disk and memory"
    );

    fs::remove_dir_all(&workspace).ok();
}

#[test]
fn import_cache_bounds_memory_entries_with_lru_eviction() {
    use import_lens_daemon::cache::memory::MAX_MEMORY_ENTRIES;
    let cache = ImportCache::new(None, false);

    for index in 0..(MAX_MEMORY_ENTRIES + 50) {
        cache.insert(format!("pkg-{index}@1.0.0::default"), result("pkg", false));
    }

    assert!(cache.memory_len() <= MAX_MEMORY_ENTRIES);
}

#[test]
fn import_cache_invalidates_multiple_packages_in_one_pass() {
    let cache = ImportCache::default();
    cache.insert("react@18.3.1::default".to_owned(), result("react", false));
    cache.insert("vue@3.4.0::ref".to_owned(), result("vue", false));
    cache.insert(
        "lodash-es@4.17.21::debounce".to_owned(),
        result("lodash-es", false),
    );

    let packages = std::collections::HashSet::from(["react".to_owned(), "vue".to_owned()]);
    cache.invalidate_packages(&packages);

    assert!(cache.get("react@18.3.1::default").is_none());
    assert!(cache.get("vue@3.4.0::ref").is_none());
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

#[test]
fn get_if_fresh_cold_daemon_serves_fresh_but_never_serves_unknown() {
    // §4.5 / Finding 13b (cold daemon): `importlens check` starts a FRESH daemon
    // that HYDRATES a prior run's DISK cache. A disk entry whose dependency can no
    // longer be verified (`Freshness::Unknown` — a transient stat/read error) must
    // never reach CI as a verified `cache_hit`. `get_if_fresh` is the unified
    // "serve only if disk-verified Fresh" read the force-fresh path uses: it returns
    // None on Unknown across BOTH the memory working set and the disk cache, so the
    // caller recomputes. The control — a genuinely Fresh cold disk entry — must
    // still be served, so the fix does not over-recompute.
    use import_lens_daemon::cache::key::{file_fingerprint_with_hash, fingerprints_for_paths};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("il-getiffresh-{}-{nanos}", std::process::id()));
    let shard = root.join("shard");
    fs::create_dir_all(&root).expect("temp root");

    // Two dependencies: one stays valid (the Fresh control); one is swapped for a
    // directory after seeding so its verification hits a non-`NotFound` read error
    // (`Unknown` — the deterministic B3/B4 directory technique, no mocking).
    let fresh_dep = root.join("fresh_dep.js");
    fs::write(&fresh_dep, "export const a = 1;").expect("fresh dep");
    let unknown_dep = root.join("unknown_dep.js");
    fs::write(&unknown_dep, "export const b = 2;").expect("unknown dep");

    // The Unknown entry needs a fingerprint carrying a CONTENT HASH: only then does
    // a later mtime/len mismatch fall through to `fs::read` (which fails on a
    // directory → Unknown). A hashless fingerprint would classify the change as
    // Stale (evict), not Unknown. The hash value is irrelevant — the directory read
    // fails before any comparison.
    let unknown_fp = vec![file_fingerprint_with_hash(&unknown_dep, Some(0x1234_5678)).expect("fp")];
    let fresh_fp = fingerprints_for_paths([fresh_dep.clone()]);

    // Seed the DISK cache, then DROP it so the redb file is flushed and closed (a
    // real cold daemon reopens the shard fresh, with nothing in the working set).
    {
        let seed = ImportCache::new(Some(shard.clone()), true);
        seed.insert_with_fingerprints("v3:fresh".to_owned(), result("fresh", false), fresh_fp);
        seed.insert_with_fingerprints(
            "v3:unknown".to_owned(),
            result("unknown", false),
            unknown_fp,
        );
        seed.flush_to_disk().expect("seed flush");
    }

    // Make the unknown dep unverifiable: swap the file for a directory at its path.
    fs::remove_file(&unknown_dep).expect("remove unknown dep file");
    fs::create_dir(&unknown_dep).expect("unknown dep becomes a directory");

    // COLD daemon: a fresh cache over the same shard with preload DISABLED, so the
    // entries live on disk but NOT in the memory working set — the exact cold-CI
    // state where the old force-fresh path fell through to `cache.get`'s disk
    // hydration and served the Unknown.
    let cold = ImportCache::new_with_recent_preload_limit(Some(shard.clone()), true, 0);
    assert_eq!(
        cold.memory_len(),
        0,
        "cold daemon must start with an empty working set"
    );

    // Control: the genuinely Fresh disk entry IS served (no over-recompute).
    let fresh_hit = cold.get_if_fresh("v3:fresh");
    assert!(
        fresh_hit.as_ref().is_some_and(|hit| hit.cache_hit),
        "a cold, disk-verified Fresh entry must be served by get_if_fresh: {fresh_hit:?}"
    );

    // The fix: the cold, disk-hydrated Unknown entry must NOT be served — every
    // non-Fresh state yields None so the force-fresh caller recomputes.
    assert!(
        cold.get_if_fresh("v3:unknown").is_none(),
        "get_if_fresh must never serve an Unknown (unverified) disk entry (§4.5)"
    );

    // ...and it must KEEP (never delete) the Unknown entry. The normal evicting read
    // still finds+serves it from disk (Unknown → keep), which both proves
    // get_if_fresh left the disk copy intact AND documents the exact laundering — a
    // `cache_hit` on an unverified value — that the force-fresh path must avoid.
    let laundered = cold.get("v3:unknown");
    assert!(
        laundered.as_ref().is_some_and(|hit| hit.cache_hit),
        "the normal get keeps+serves Unknown unchanged; get_if_fresh must not delete it: {laundered:?}"
    );

    drop(cold);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn disk_hydration_interactive_get_promotes_recency() {
    // Finding 10b / §3.2: a memory miss + disk hit must promote recency for an
    // INTERACTIVE `get` (the disk-hydrated entry is about to be the working
    // set's most-recently-used one), but must leave the persisted `last_seq`
    // alone for non-promoting reads (prewarm / force-fresh) — otherwise a
    // just-accessed rehydrated entry stays a prime eviction victim.
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("il-diskhydrate-get-{}-{nanos}", std::process::id()));
    let shard = root.join("shard");
    fs::create_dir_all(&root).expect("temp root");

    let interactive_key = "v4:diskhydrate-interactive";
    let prewarm_key = "v4:diskhydrate-prewarm";
    let forcefresh_key = "v4:diskhydrate-forcefresh";

    // Seed three DISK-only entries. Each `insert` stamps a fresh, strictly
    // increasing `last_seq`, so each key's pre-hydration persisted value
    // (read back below) is known and distinct.
    {
        let seed = ImportCache::new(Some(shard.clone()), true);
        seed.insert(interactive_key.to_owned(), result("interactive-pkg", false));
        seed.insert(prewarm_key.to_owned(), result("prewarm-pkg", false));
        seed.insert(forcefresh_key.to_owned(), result("forcefresh-pkg", false));
        seed.flush_to_disk().expect("seed flush");
    }

    let interactive_before = disk_persisted_last_seq(&shard, interactive_key);
    let prewarm_before = disk_persisted_last_seq(&shard, prewarm_key);
    let forcefresh_before = disk_persisted_last_seq(&shard, forcefresh_key);

    // Cold reopen with preload DISABLED: none of the three keys are
    // memory-resident, so every read below goes through the disk-hydration
    // branch of `read`, never the memory-hit branch.
    let cold = ImportCache::new_with_recent_preload_limit(Some(shard.clone()), true, 0);
    assert_eq!(
        cold.memory_len(),
        0,
        "cold reopen must start with an empty working set"
    );

    // INTERACTIVE (promoting) disk-hydration hit.
    assert!(cold.get(interactive_key).is_some());
    // Non-promoting disk-hydration hits — controls (must NOT regress C2/B4b).
    assert!(cold.get_for_prewarm(prewarm_key).is_some());
    assert!(cold.get_if_fresh(forcefresh_key).is_some());

    // `flush_to_disk`'s recency sweep re-persists anything promoted since its
    // last persist, so the after-state can be read straight back off disk.
    cold.flush_to_disk().expect("flush after reads");
    drop(cold);

    let interactive_after = disk_persisted_last_seq(&shard, interactive_key);
    let prewarm_after = disk_persisted_last_seq(&shard, prewarm_key);
    let forcefresh_after = disk_persisted_last_seq(&shard, forcefresh_key);

    assert!(
        interactive_after > interactive_before,
        "an interactive disk-hydration hit must promote last_seq to a fresh value: \
         {interactive_before} -> {interactive_after}"
    );
    assert_eq!(
        prewarm_after, prewarm_before,
        "a prewarm disk-hydration hit must NOT promote last_seq"
    );
    assert_eq!(
        forcefresh_after, forcefresh_before,
        "a force-fresh disk-hydration hit must NOT promote last_seq"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn disk_hydration_interactive_swr_read_promotes_recency() {
    // Same bug (Finding 10b / §3.2), for the C2 stale-while-revalidate read
    // path: `get_with_result_freshness` (interactive) must promote a disk
    // hydration hit; `get_with_result_freshness_for_bulk` must not.
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("il-diskhydrate-swr-{}-{nanos}", std::process::id()));
    let shard = root.join("shard");
    fs::create_dir_all(&root).expect("temp root");

    let interactive_key = "v4:diskhydrate-swr-interactive";
    let bulk_key = "v4:diskhydrate-swr-bulk";

    {
        let seed = ImportCache::new(Some(shard.clone()), true);
        seed.insert(interactive_key.to_owned(), result("interactive-pkg", false));
        seed.insert(bulk_key.to_owned(), result("bulk-pkg", false));
        seed.flush_to_disk().expect("seed flush");
    }

    let interactive_before = disk_persisted_last_seq(&shard, interactive_key);
    let bulk_before = disk_persisted_last_seq(&shard, bulk_key);

    // Cold reopen with preload DISABLED: neither key is memory-resident, so
    // both reads below go through `read_with_result_freshness`'s disk-hydration
    // branch, never its memory-hit branch.
    let cold = ImportCache::new_with_recent_preload_limit(Some(shard.clone()), true, 0);
    assert_eq!(
        cold.memory_len(),
        0,
        "cold reopen must start with an empty working set"
    );

    // Interactive (promoting) SWR disk-hydration hit.
    assert!(cold.get_with_result_freshness(interactive_key).is_some());
    // Bulk (non-promoting) SWR disk-hydration hit — control.
    assert!(cold.get_with_result_freshness_for_bulk(bulk_key).is_some());

    cold.flush_to_disk().expect("flush after reads");
    drop(cold);

    let interactive_after = disk_persisted_last_seq(&shard, interactive_key);
    let bulk_after = disk_persisted_last_seq(&shard, bulk_key);

    assert!(
        interactive_after > interactive_before,
        "an interactive SWR disk-hydration hit must promote last_seq to a fresh value: \
         {interactive_before} -> {interactive_after}"
    );
    assert_eq!(
        bulk_after, bulk_before,
        "a bulk SWR disk-hydration hit must NOT promote last_seq"
    );

    fs::remove_dir_all(&root).ok();
}
