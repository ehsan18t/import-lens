use import_lens_daemon::{
    cache::{
        disk::DiskCache,
        key::fingerprints_for_paths,
        memory::CachedImport,
        project::{
            ProjectCacheRegistry, normalize_project_root, project_cache_shard_id,
            remove_legacy_central_cache,
        },
        recency::RecencyClock,
    },
    ipc::protocol::{ConfidenceLevel, ImportResult},
};
use redb::{Database, ReadableDatabase, TableDefinition};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, atomic::AtomicU64},
    thread,
    time::Duration,
};

mod common;

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const SHARD_METADATA_FILE_NAME: &str = "importlens-project-cache.json";

fn result(specifier: &str) -> ImportResult {
    let mut result = ImportResult::measured(
        specifier,
        import_lens_daemon::ipc::protocol::MeasuredSizes {
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
    result
}

fn cached(specifier: &str) -> CachedImport {
    CachedImport {
        result: result(specifier),
        dependency_fingerprints: Vec::new(),
        verified_generation: 0,
        verified_at: None,
        first_party: false,
        last_seq: Arc::new(AtomicU64::new(1)),
        persisted_seq: Arc::new(AtomicU64::new(1)),
    }
}

/// C5 / Finding 10d (§3.3): the startup recency seed must lift the process-global
/// recency clock above the GLOBAL maximum persisted seq before the server serves
/// any request. A shard left on disk by a prior session with a large `max_seq` must
/// be observed even though it is never loaded this session — otherwise a fresh
/// post-restart access (small seq) sorts as OLDER than that untouched shard, and the
/// evictor's smallest-`oldest_seq` victim selection inverts LRU across restarts.
///
/// The persisted `max_seq` is written ~2M above the live clock via a direct
/// `DiskCache` insert (which stamps the entry's `last_seq` verbatim WITHOUT touching
/// the process-global clock), so the seed has real work to do: until it runs, the
/// clock stays far below the persisted high-water. `next_seq() > future_seq` can
/// therefore only hold if the seed observed the shard — no serial lock is needed
/// because no concurrent sibling could advance the clock by two million.
#[test]
fn startup_seed_advances_recency_clock_past_persisted_shard_max_seq() {
    let base = common::temp_workspace("import-lens-project-cache-startup-seed");
    let project_root = base.join("prior-session-app");
    fs::create_dir_all(&project_root).expect("project root should exist");

    // Create the shard the way production does, so it carries the valid metadata
    // file that the registry's shard enumeration discovers. Dropping the registry
    // closes the shard's database file so the direct insert below can reopen it.
    {
        let setup = ProjectCacheRegistry::new(Some(base.clone()), true, 512);
        setup.cache_for_root(&project_root);
    }

    // Persist an entry stamped far ABOVE the live clock, simulating a prior session
    // that accumulated millions of accesses.
    let future_seq = RecencyClock::next_seq() + 2_000_000;
    {
        let shard_dir = base.join(project_cache_shard_id(&project_root));
        let disk = DiskCache::new(Some(shard_dir), true);
        let mut entry = cached("react");
        entry.last_seq = Arc::new(AtomicU64::new(future_seq));
        disk.insert("react@18.3.1::default", &entry)
            .expect("insert should queue");
        disk.flush_pending_inserts();
    }

    // A fresh registry — as a post-restart daemon builds at Hello — seeds the clock
    // from the on-disk shard summaries BEFORE any request is served.
    let registry = ProjectCacheRegistry::new(Some(base.clone()), true, 512);
    registry.seed_recency_clock_from_disk();

    assert!(
        RecencyClock::next_seq() > future_seq,
        "startup seed must advance the recency clock past the persisted shard \
         max_seq ({future_seq}) so a new entry sorts newer than a prior-session shard"
    );

    drop(registry);
    fs::remove_dir_all(base).expect("cleanup");
}

#[test]
fn project_cache_shard_id_is_stable_for_normalized_project_root() {
    let root = PathBuf::from("C:/Workspace/App");

    assert_eq!(project_cache_shard_id(&root), project_cache_shard_id(&root));
    assert!(
        project_cache_shard_id(&root).starts_with("v1-"),
        "unexpected shard id"
    );
    assert_eq!(
        normalize_project_root(&PathBuf::from("C:\\Workspace\\App\\")),
        "c:/workspace/app"
    );
}

#[test]
fn purge_orphans_removes_shards_for_deleted_project_roots() {
    let storage = common::temp_workspace("import-lens-project-cache-purge-orphans");
    let live_root = storage.join("live-app");
    let dead_root = storage.join("dead-app");
    fs::create_dir_all(&live_root).expect("live root should exist");
    // dead_root is intentionally never created: the project was deleted.
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    registry
        .cache_for_root(&live_root)
        .insert("react@18.3.1::default".to_owned(), result("react"));
    registry
        .cache_for_root(&dead_root)
        .insert("vue@3.4.0::default".to_owned(), result("vue"));

    let (removed, _scrubbed) = registry.purge_orphans();
    let shards = registry.list_shards();

    fs::remove_dir_all(storage).expect("temp storage should be removed");
    assert!(
        removed
            .iter()
            .any(|op| op.removed && op.project_root == dead_root.to_string_lossy()),
        "the deleted project's shard should be purged"
    );
    assert!(
        shards
            .iter()
            .any(|shard| shard.project_root == live_root.to_string_lossy()),
        "the live project's shard should survive"
    );
    assert!(
        !shards
            .iter()
            .any(|shard| shard.project_root == dead_root.to_string_lossy()),
        "the deleted project's shard should no longer be listed"
    );
}

// RB-17: the automatic maintenance-tick sweep reclaims an abandoned project's
// whole shard (the case on-access reclaim never reaches), keeps live shards, and
// throttles so it doesn't rescan every tick.
#[test]
fn sweep_orphaned_shards_reclaims_deleted_projects_and_throttles() {
    let storage = common::temp_workspace("import-lens-project-cache-orphan-sweep");
    let live_root = storage.join("live-app");
    let dead_root = storage.join("dead-app");
    fs::create_dir_all(&live_root).expect("live root should exist");
    // dead_root is never created — the project was moved/deleted.
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    registry
        .cache_for_root(&live_root)
        .insert("react@18.3.1::default".to_owned(), result("react"));
    registry
        .cache_for_root(&dead_root)
        .insert("vue@3.4.0::default".to_owned(), result("vue"));

    let first = registry.sweep_orphaned_shards_if_due();
    // A second call immediately after is throttled to a no-op (returns empty)
    // even though nothing else changed on disk.
    let second = registry.sweep_orphaned_shards_if_due();
    let shards = registry.list_shards();

    fs::remove_dir_all(storage).expect("temp storage should be removed");
    assert!(
        first
            .iter()
            .any(|op| op.removed && op.project_root == dead_root.to_string_lossy()),
        "the sweep should reclaim the abandoned project's shard"
    );
    assert!(
        second.is_empty(),
        "a second sweep within the throttle window must be a no-op: {second:?}"
    );
    assert!(
        shards
            .iter()
            .any(|shard| shard.project_root == live_root.to_string_lossy()),
        "the live project's shard must survive the sweep"
    );
    assert!(
        !shards
            .iter()
            .any(|shard| shard.project_root == dead_root.to_string_lossy()),
        "the abandoned project's shard must no longer be listed"
    );
}

// Guards the highest-stakes reclaim site (X-3): `purge_orphans` deletes a
// non-orphan shard with a whole-`remove_dir_all`, so a false "gone" verdict for
// a project root that merely failed to stat transiently would destroy an
// entire project's cache. This asserts the shard directory survives ON DISK
// (not just absent from `removed`/`list_shards`) when its project root exists.
#[test]
fn purge_orphans_does_not_remove_shard_whose_project_root_still_exists() {
    let storage = common::temp_workspace("import-lens-project-cache-purge-existing-root");
    let live_root = storage.join("still-here-app");
    fs::create_dir_all(&live_root).expect("live root should exist");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    registry
        .cache_for_root(&live_root)
        .insert("react@18.3.1::default".to_owned(), result("react"));
    registry.flush_to_disk().expect("flush should succeed");

    let shard_id = project_cache_shard_id(&live_root);
    let shard_dir = storage.join(&shard_id);
    assert!(
        shard_dir.is_dir(),
        "fixture setup should have created the shard directory on disk"
    );

    let (removed, _scrubbed) = registry.purge_orphans();

    assert!(
        !removed
            .iter()
            .any(|op| op.shard_id == shard_id && op.removed),
        "a shard whose project root still exists must not be reported as removed: {removed:?}"
    );
    assert!(
        shard_dir.is_dir(),
        "purge_orphans must not remove_dir_all a shard whose project root still exists on disk"
    );
    assert!(
        registry
            .list_shards()
            .iter()
            .any(|shard| shard.shard_id == shard_id),
        "the surviving shard should still be listed"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn project_cache_registry_stores_projects_in_separate_shards() {
    let storage = common::temp_workspace("import-lens-project-cache");
    let first_root = storage.join("first-app");
    let second_root = storage.join("second-app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    registry
        .cache_for_root(&first_root)
        .insert("react@18.3.1::default".to_owned(), result("react"));
    registry
        .cache_for_root(&second_root)
        .insert("vue@3.4.0::default".to_owned(), result("vue"));

    let shards = registry.list_shards();
    let first = shards
        .iter()
        .find(|shard| shard.project_root == first_root.to_string_lossy())
        .expect("first project shard should be listed");
    let second = shards
        .iter()
        .find(|shard| shard.project_root == second_root.to_string_lossy())
        .expect("second project shard should be listed");

    assert_ne!(first.shard_id, second.shard_id);
    assert_ne!(first.cache_path, second.cache_path);
    assert!(PathBuf::from(&first.cache_path).starts_with(&storage));
    assert!(PathBuf::from(&second.cache_path).starts_with(&storage));

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn project_cache_registry_throttles_loaded_shard_metadata_writes() {
    let storage = common::temp_workspace("import-lens-project-cache-metadata-throttle");
    let project_root = storage.join("app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
    let shard_id = project_cache_shard_id(&project_root);
    let metadata_path = storage.join(&shard_id).join(SHARD_METADATA_FILE_NAME);

    registry.cache_for_root(&project_root);
    let first_metadata_last_used = metadata_last_used_millis(&metadata_path);

    thread::sleep(Duration::from_millis(20));
    registry.cache_for_root(&project_root);

    let second_metadata_last_used = metadata_last_used_millis(&metadata_path);
    let status_last_used = registry
        .status_for_root(Some(&project_root))
        .current_project
        .and_then(|shard| shard.last_used_millis)
        .expect("loaded project should have last-used metadata");

    assert_eq!(second_metadata_last_used, first_metadata_last_used);
    assert!(status_last_used > first_metadata_last_used);

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn project_cache_registry_invalidates_unloaded_shards_without_recent_preload() {
    let storage = common::temp_workspace("import-lens-project-cache-unloaded-invalidate");
    let project_root = storage.join("app");
    let stale_dependency = storage.join("stale-dependency.js");
    fs::write(&stale_dependency, b"stale fixture")
        .expect("stale dependency fixture should be written");

    {
        let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
        let cache = registry.cache_for_root(&project_root);
        cache.insert_with_fingerprints(
            "vue@3.4.0::default".to_owned(),
            result("vue"),
            fingerprints_for_paths([stale_dependency.clone()]),
        );
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
    }

    fs::remove_file(&stale_dependency).expect("stale dependency fixture should be removed");

    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
    registry.invalidate_package("react");

    let cache_path = storage.join(project_cache_shard_id(&project_root));
    let db_path = cache_path.join(CACHE_DB_FILE_NAME);
    assert!(!disk_cache_entry_exists(&db_path, "react@18.3.1::default"));
    assert!(disk_cache_entry_exists(&db_path, "vue@3.4.0::default"));

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn project_cache_registry_invalidates_multiple_packages_in_one_pass() {
    let storage = common::temp_workspace("import-lens-project-cache-batch-invalidate");
    let project_root = storage.join("app");

    {
        let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
        let cache = registry.cache_for_root(&project_root);
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
        cache.insert("vue@3.4.0::default".to_owned(), result("vue"));
        cache.insert("lodash@4.17.21::default".to_owned(), result("lodash"));
    }

    // Fresh registry: the shard is on disk (unloaded), exercising the batched
    // disk-shard invalidation path.
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);
    registry.invalidate_packages(&["react".to_owned(), "vue".to_owned()]);

    let cache_path = storage.join(project_cache_shard_id(&project_root));
    let db_path = cache_path.join(CACHE_DB_FILE_NAME);
    assert!(!disk_cache_entry_exists(&db_path, "react@18.3.1::default"));
    assert!(!disk_cache_entry_exists(&db_path, "vue@3.4.0::default"));
    assert!(disk_cache_entry_exists(&db_path, "lodash@4.17.21::default"));

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

// Fix (a) guard (Finding 11): `invalidate_packages` snapshots the loaded shards'
// Arcs, releases `loaded`, then runs the per-shard redb writes off-lock. This
// asserts the correctness that must survive that change — a LOADED shard still has
// exactly the named packages invalidated and the others preserved. The shard is
// kept resident (registry alive, handle held) so the loaded-Arc path is exercised,
// not the unloaded temp-open path the sibling tests cover.
#[test]
fn invalidate_packages_on_a_loaded_shard_drops_only_the_named_packages() {
    let storage = common::temp_workspace("import-lens-project-cache-loaded-invalidate");
    let project_root = storage.join("app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    let cache = registry.cache_for_root(&project_root);
    cache.insert("react@18.3.1::default".to_owned(), result("react"));
    cache.insert("vue@3.4.0::default".to_owned(), result("vue"));
    cache.insert("lodash@4.17.21::default".to_owned(), result("lodash"));

    registry.invalidate_packages(&["react".to_owned(), "vue".to_owned()]);

    assert!(
        cache.get("react@18.3.1::default").is_none(),
        "the loaded shard must have react invalidated"
    );
    assert!(
        cache.get("vue@3.4.0::default").is_none(),
        "the loaded shard must have vue invalidated"
    );
    assert!(
        cache.get("lodash@4.17.21::default").is_some(),
        "an unrelated package must survive invalidation"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

// Fix (b) guard (Finding 11): the cold path opens + registers the shard under a
// per-shard load lock and re-acquires `loaded` briefly to insert. This asserts the
// double-check/insert works: a cold load followed by a warm hit returns the SAME
// registered Arc (the warm path finds what the cold path stored), and the cold
// load opened real persistence.
#[test]
fn cold_load_then_warm_hit_return_the_same_shard_arc() {
    let storage = common::temp_workspace("import-lens-project-cache-same-arc");
    let project_root = storage.join("app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    let cold = registry.cache_for_root(&project_root);
    let warm = registry.cache_for_root(&project_root);

    assert!(
        Arc::ptr_eq(&cold, &warm),
        "a warm hit must return the same registered Arc the cold load created"
    );
    assert!(
        cold.disk_available(),
        "the cold load must open real persistence, not degrade"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

// Fix (b) guard (Finding 11): two threads cold-load DIFFERENT projects at once.
// With `loaded` no longer held across shard I/O they proceed in parallel; the test
// asserts the outcome invariant that must hold regardless — both get a working,
// distinct, registered shard (no deadlock, no lost shard, no cross-talk).
#[test]
fn concurrent_cold_loads_of_distinct_projects_each_get_a_working_shard() {
    let storage = common::temp_workspace("import-lens-project-cache-parallel-distinct");
    let root_a = storage.join("app-a");
    let root_b = storage.join("app-b");
    let registry = Arc::new(ProjectCacheRegistry::new(Some(storage.clone()), true, 512));

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [root_a.clone(), root_b.clone()]
        .into_iter()
        .map(|root| {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let cache = registry.cache_for_root(&root);
                assert!(
                    cache.disk_available(),
                    "each parallel cold load must persist"
                );
                cache
            })
        })
        .collect();
    let caches: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("cold load thread should not panic"))
        .collect();

    assert!(
        !Arc::ptr_eq(&caches[0], &caches[1]),
        "distinct projects must get distinct shard caches"
    );
    let shards = registry.list_shards();
    assert!(
        shards
            .iter()
            .any(|shard| shard.project_root == root_a.to_string_lossy()),
        "the first project's shard should be registered"
    );
    assert!(
        shards
            .iter()
            .any(|shard| shard.project_root == root_b.to_string_lossy()),
        "the second project's shard should be registered"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

// Fix (b) core guard (Finding 11): two threads race a cold load of the SAME
// project. The per-shard load lock + double-check must open the DB exactly once
// and hand BOTH racers the one registered Arc. This outcome holds in every
// interleaving: if they truly race, the load lock serializes them and the loser's
// double-check returns the winner's Arc; if one finishes first, the other is a
// plain warm hit. A racy double-open would instead give one thread a distinct,
// degraded (DatabaseAlreadyOpen -> memory-only) Arc — failing both asserts — so
// this has teeth against the exact failure mode the refactor could introduce.
#[test]
fn racing_cold_loads_of_the_same_project_open_the_db_once() {
    let storage = common::temp_workspace("import-lens-project-cache-race-same");
    let project_root = storage.join("app");
    let registry = Arc::new(ProjectCacheRegistry::new(Some(storage.clone()), true, 512));

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let root = project_root.clone();
            thread::spawn(move || {
                barrier.wait();
                registry.cache_for_root(&root)
            })
        })
        .collect();
    let caches: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("racing cold load thread should not panic")
        })
        .collect();

    assert!(
        Arc::ptr_eq(&caches[0], &caches[1]),
        "racing same-project cold loads must return one shared shard Arc (opened once)"
    );
    assert!(
        caches[0].disk_available(),
        "the single shared open must provide persistence, not a degraded fallback"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn project_cache_registry_removes_current_project_without_removing_other_shards() {
    let storage = common::temp_workspace("import-lens-project-cache-remove");
    let first_root = storage.join("first-app");
    let second_root = storage.join("second-app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    registry
        .cache_for_root(&first_root)
        .insert("react@18.3.1::default".to_owned(), result("react"));
    registry
        .cache_for_root(&second_root)
        .insert("vue@3.4.0::default".to_owned(), result("vue"));

    let removed = registry.remove_current_project(&first_root);
    assert_eq!(removed.len(), 1);
    assert!(removed[0].removed, "{removed:?}");

    let shards = registry.list_shards();
    assert!(
        !shards
            .iter()
            .any(|shard| shard.project_root == first_root.to_string_lossy())
    );
    assert!(
        shards
            .iter()
            .any(|shard| shard.project_root == second_root.to_string_lossy())
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

fn metadata_last_used_millis(path: &Path) -> u64 {
    let contents = fs::read_to_string(path).expect("project cache metadata should exist");
    let metadata: serde_json::Value =
        serde_json::from_str(&contents).expect("project cache metadata should parse");
    metadata["last_used_millis"]
        .as_u64()
        .expect("project cache metadata should include last_used_millis")
}

fn disk_cache_entry_exists(db_path: &Path, key: &str) -> bool {
    let db = Database::open(db_path).expect("cache database should open");
    let read_txn = db
        .begin_read()
        .expect("cache read transaction should begin");
    let table = read_txn
        .open_table(CACHE_TABLE)
        .expect("cache table should open");
    table
        .get(key)
        .expect("cache table lookup should succeed")
        .is_some()
}

#[test]
fn project_cache_registry_removes_legacy_central_cache_file() {
    let storage = common::temp_workspace("import-lens-project-cache-legacy");
    let legacy_cache_path = storage.join("importlens.redb");
    fs::write(&legacy_cache_path, b"legacy cache").expect("legacy cache fixture should be written");

    let result = remove_legacy_central_cache(&storage).expect("legacy cache should be reported");

    assert!(result.removed, "{result:?}");
    assert_eq!(result.shard_id, "legacy-central");
    assert_eq!(
        result.cache_path,
        legacy_cache_path.to_string_lossy().to_string()
    );
    assert!(!legacy_cache_path.exists());
    assert!(remove_legacy_central_cache(&storage).is_none());

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn byte_budget_evicts_oldest_entries_across_shards_respecting_the_floor() {
    use import_lens_daemon::cache::budget::EVICTION_FLOOR;

    let storage = common::temp_workspace("import-lens-project-cache-budget");
    let root_a = storage.join("app-a");
    let root_b = storage.join("app-b");
    fs::create_dir_all(&root_a).expect("root a");
    fs::create_dir_all(&root_b).expect("root b");

    // 40 KB budget; ~1000 tiny entries (~62 B each ≈ 62 KB) blow past it, and
    // eviction can reach the low-water mark without hitting both shards' floors.
    let budget_bytes = 40 * 1024;
    let registry =
        ProjectCacheRegistry::new_with_budget_bytes(Some(storage.clone()), true, 512, budget_bytes);

    let per_shard = 500;
    let cache_a = registry.cache_for_root(&root_a);
    for index in 0..per_shard {
        cache_a.insert(format!("a{index}@1.0.0::default"), result("a"));
    }
    let cache_b = registry.cache_for_root(&root_b);
    for index in 0..per_shard {
        cache_b.insert(format!("b{index}@1.0.0::default"), result("b"));
    }
    registry.flush_to_disk().expect("flush should succeed");

    let total_before = cache_a.shard_rollup().total_bytes + cache_b.shard_rollup().total_bytes;
    assert!(
        total_before > budget_bytes,
        "test fixture must exceed the budget ({total_before} <= {budget_bytes})"
    );

    let outcome = registry.evict_to_budget();
    assert!(outcome.evicted_bytes > 0, "eviction must free bytes");

    let rollup_a = cache_a.shard_rollup();
    let rollup_b = cache_b.shard_rollup();
    let total_after = rollup_a.total_bytes + rollup_b.total_bytes;

    assert!(
        total_after <= budget_bytes,
        "eviction must bring the total under budget: {total_after} > {budget_bytes}"
    );
    assert!(
        rollup_a.entry_count >= EVICTION_FLOOR && rollup_b.entry_count >= EVICTION_FLOOR,
        "each shard keeps at least its floor of newest entries"
    );
    // Shard a holds the globally-oldest seqs (inserted first). Reaching the low
    // water needs more bytes than a's evictable set alone, so a must drain to
    // EXACTLY its floor before eviction moves on to b.
    assert_eq!(
        rollup_a.entry_count, EVICTION_FLOOR,
        "the globally-oldest shard drains to exactly its floor"
    );
    assert!(
        rollup_a.entry_count <= rollup_b.entry_count,
        "the older shard is evicted first"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn byte_budget_pages_past_memory_hot_keys_instead_of_retiring_the_shard() {
    use import_lens_daemon::cache::budget::{EVICTION_BATCH, EVICTION_FLOOR};

    let storage = common::temp_workspace("import-lens-project-cache-hot-paging");
    let root = storage.join("app");
    fs::create_dir_all(&root).expect("root");

    // One shard. Layout by persisted seq: the lowest EVICTION_BATCH are made
    // memory-hot, a large cold region sits above them, and the newest FLOOR are
    // floor-protected. FLOOR + 6*BATCH keeps the cold evictable region (~5*BATCH)
    // far larger than what the low-water mark forces us to evict, so the budget
    // is reachable by cold victims ALONE — but only if eviction pages past the
    // all-hot lowest batch instead of retiring the shard on it (Finding 10c).
    let entry_count = EVICTION_FLOOR as usize + EVICTION_BATCH * 6;
    let key_of = |index: usize| format!("pkg{index:05}@1.0.0::default");

    // Phase 1: populate + persist with eviction disabled (budget 0), then measure
    // the persisted total. Every value is identical in size (same envelope, fixed
    // 8-byte seq prefix), so the budget below evicts a size-independent fraction.
    let total_bytes = {
        let registry =
            ProjectCacheRegistry::new_with_budget_bytes(Some(storage.clone()), true, 512, 0);
        let cache = registry.cache_for_root(&root);
        for index in 0..entry_count {
            cache.insert(key_of(index), result("pkg"));
        }
        registry
            .flush_to_disk()
            .expect("flush should persist the seqs");
        cache.shard_rollup().total_bytes
    };
    assert!(total_bytes > 0, "the fixture must persist bytes");

    // Budget 70% of the persisted total -> low water 63% -> eviction must free
    // ~37% of the entries (~3 batches). That is well above the 2*BATCH hot+floor
    // protected entries yet well below the ~5*BATCH cold evictable entries.
    let budget_bytes = total_bytes * 7 / 10;

    let registry =
        ProjectCacheRegistry::new_with_budget_bytes(Some(storage.clone()), true, 512, budget_bytes);
    let cache = registry.cache_for_root(&root);

    // Make the lowest EVICTION_BATCH persisted-seq entries memory-hot: an
    // interactive get hydrates + promotes them (last_seq past the persisted seq),
    // with no flush, so the disk index still ranks them lowest while memory knows
    // they are the live working set.
    let hot: Vec<String> = (0..EVICTION_BATCH).map(key_of).collect();
    for key in &hot {
        assert!(cache.get(key).is_some(), "hot fixture entry should hydrate");
    }

    let outcome = registry.evict_to_budget();

    assert!(
        outcome.evicted_bytes > 0,
        "eviction must free genuinely-cold entries deeper in the shard, \
         not retire on the all-hot lowest batch"
    );
    assert!(
        !outcome.still_over_budget,
        "the low-water mark is reachable by cold victims alone: {outcome:?}"
    );

    let rollup = cache.shard_rollup();
    assert!(
        rollup.total_bytes <= budget_bytes,
        "eviction must bring the shard within budget: {} > {budget_bytes}",
        rollup.total_bytes
    );
    assert!(
        rollup.entry_count >= EVICTION_FLOOR,
        "the newest floor entries survive"
    );
    // The memory-hot working set must be shielded end to end: every promoted
    // entry must still be served after the eviction pass.
    for key in &hot {
        assert!(
            cache.get(key).is_some(),
            "a memory-hot entry must survive eviction: {key}"
        );
    }

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn a_racing_shard_open_degrades_one_call_and_heals_on_the_next() {
    let storage = common::temp_workspace("import-lens-project-cache-open-race");
    let root = storage.join("app");
    fs::create_dir_all(&root).expect("project root");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512);

    // Simulate a maintenance pass (eviction / invalidation / orphan purge)
    // holding this shard's redb file when the project loads: redb allows one
    // Database per file per process, so the load's open fails.
    let shard_dir = storage.join(project_cache_shard_id(&root));
    fs::create_dir_all(&shard_dir).expect("shard dir");
    let held = Database::create(shard_dir.join(CACHE_DB_FILE_NAME)).expect("hold the db file");

    // The racing call serves a memory-only cache and must NOT register it:
    // registering would silently disable this project's persistence forever.
    let degraded = registry.cache_for_root(&root);
    assert!(
        !degraded.disk_available(),
        "the racing open must degrade to memory-only"
    );

    // Once the maintenance pass releases the file, the next call must retry the
    // open and come back with real persistence.
    drop(held);
    let healed = registry.cache_for_root(&root);
    assert!(
        healed.disk_available(),
        "the shard must heal on the next load instead of staying disabled"
    );
    healed.insert("react@18.3.1::default".to_owned(), result("react"));
    healed.flush_to_disk().expect("flush should succeed");
    assert!(healed.get("react@18.3.1::default").is_some());

    drop((degraded, healed, registry));
    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn byte_budget_evicts_unloaded_shards_and_shrinks_their_files() {
    use import_lens_daemon::cache::budget::EVICTION_FLOOR;

    let storage = common::temp_workspace("import-lens-project-cache-unloaded-budget");
    let root_a = storage.join("app-a");
    let root_b = storage.join("app-b");
    fs::create_dir_all(&root_a).expect("root a");
    fs::create_dir_all(&root_b).expect("root b");

    let budget_bytes = 40 * 1024;
    {
        let registry = ProjectCacheRegistry::new_with_budget_bytes(
            Some(storage.clone()),
            true,
            512,
            budget_bytes,
        );
        let cache_a = registry.cache_for_root(&root_a);
        for index in 0..500 {
            cache_a.insert(format!("a{index}@1.0.0::default"), result("a"));
        }
        let cache_b = registry.cache_for_root(&root_b);
        for index in 0..500 {
            cache_b.insert(format!("b{index}@1.0.0::default"), result("b"));
        }
        registry.flush_to_disk().expect("flush should succeed");
    }

    // Fresh registry, nothing loaded: production's common shape, where most of
    // the budget lives in OTHER projects' unloaded shards. The maintenance pass
    // must temp-open them, evict, and stay within budget.
    let registry =
        ProjectCacheRegistry::new_with_budget_bytes(Some(storage.clone()), true, 512, budget_bytes);
    let outcome = registry.run_maintenance(false);
    assert!(
        !outcome.skipped_under_budget,
        "the over-budget cache must not be skipped by the physical-size gate"
    );
    assert!(
        outcome.eviction.evicted_bytes > 0,
        "unloaded shards must be evicted from"
    );

    let rollup_a = registry.cache_for_root(&root_a).shard_rollup();
    let rollup_b = registry.cache_for_root(&root_b).shard_rollup();
    assert!(
        rollup_a.total_bytes + rollup_b.total_bytes <= budget_bytes,
        "the logical total must land within the budget"
    );
    assert!(
        rollup_a.entry_count >= EVICTION_FLOOR && rollup_b.entry_count >= EVICTION_FLOOR,
        "each shard keeps its floor even when evicted while unloaded"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

// Regression guard (C7 / Finding 12, §5.5): `collect_shard_targets` temp-opens
// every UNLOADED shard fresh at the START of a maintenance pass, then
// `compact_if_fragmented` runs on those same shards a few ms later in the SAME
// pass. If a just-opened shard were seeded "recently accessed", the idle gate
// would skip it forever, so a heavily-evicted cold shard — the exact case
// compaction exists for — would never be compacted. This drives the REAL
// `run_maintenance` (NOT `mark_idle_for_test`) and asserts the cold shard IS
// compacted and its file shrinks. The sibling unloaded-eviction test above never
// checks compaction, so the regression passed straight through it.
#[test]
fn maintenance_compacts_cold_fragmented_unloaded_shards() {
    let storage = common::temp_workspace("import-lens-project-cache-cold-compact");
    let root = storage.join("app");
    fs::create_dir_all(&root).expect("root");

    // Populate one shard with far more than the eviction floor, eviction OFF
    // (budget 0 so population itself never evicts), then unload it by dropping the
    // registry (its Drop flushes and closes the shard file).
    let entry_count = 1500;
    {
        let registry =
            ProjectCacheRegistry::new_with_budget_bytes(Some(storage.clone()), true, 512, 0);
        let cache = registry.cache_for_root(&root);
        for index in 0..entry_count {
            cache.insert(format!("pkg{index:05}@1.0.0::default"), result("pkg"));
        }
        registry.flush_to_disk().expect("flush should persist");
    }

    let shard_file = storage
        .join(project_cache_shard_id(&root))
        .join(CACHE_DB_FILE_NAME);
    let size_before = fs::metadata(&shard_file).expect("shard file").len();

    // Fresh registry, nothing loaded — production's common shape. A tiny budget
    // forces eviction to drain the cold shard to its floor, freeing the vast
    // majority of its pages (redb keeps them as reclaimable free space), so the
    // file crosses COMPACT_THRESHOLD. Eviction does NOT shrink a redb file, so
    // only compaction can make it smaller.
    let registry =
        ProjectCacheRegistry::new_with_budget_bytes(Some(storage.clone()), true, 512, 4 * 1024);
    let outcome = registry.run_maintenance(false);

    assert!(
        !outcome.skipped_under_budget,
        "the over-budget cache must not be skipped by the physical-size gate"
    );
    assert!(
        outcome.eviction.evicted_bytes > 0,
        "the cold unloaded shard must be evicted from"
    );
    // The heart of the regression: the cold, never-user-accessed, now-fragmented
    // shard must actually be compacted by the pass — not skipped by the idle gate
    // on its maintenance-temp-open.
    assert!(
        outcome.compacted_shards >= 1,
        "a heavily-evicted cold unloaded shard must be compacted (compacted_shards was {})",
        outcome.compacted_shards
    );
    let size_after = fs::metadata(&shard_file).expect("shard file").len();
    assert!(
        size_after < size_before,
        "compaction must shrink the cold shard's file: {size_after} >= {size_before}"
    );

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn status_reports_total_bytes_budget_and_per_project_entry_counts() {
    let storage = common::temp_workspace("import-lens-project-cache-status-counts");
    let root_a = storage.join("app-a");
    let root_b = storage.join("app-b");
    fs::create_dir_all(&root_a).expect("root a should exist");
    fs::create_dir_all(&root_b).expect("root b should exist");

    let max_size_mb = 8u64;
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, max_size_mb);

    // Shard A holds three distinct entries; shard B holds two.
    let cache_a = registry.cache_for_root(&root_a);
    cache_a.insert("pkg-a1@1.0.0::default".to_owned(), result("pkg-a1"));
    cache_a.insert("pkg-a2@1.0.0::default".to_owned(), result("pkg-a2"));
    cache_a.insert("pkg-a3@1.0.0::default".to_owned(), result("pkg-a3"));
    let cache_b = registry.cache_for_root(&root_b);
    cache_b.insert("pkg-b1@1.0.0::default".to_owned(), result("pkg-b1"));
    cache_b.insert("pkg-b2@1.0.0::default".to_owned(), result("pkg-b2"));

    // Ground truth straight from the O(1) C1 summaries of the loaded shards.
    let roll_a = cache_a.shard_rollup();
    let roll_b = cache_b.shard_rollup();
    assert_eq!(roll_a.entry_count, 3, "shard A should hold three entries");
    assert_eq!(roll_b.entry_count, 2, "shard B should hold two entries");

    let status = registry.status_for_root(Some(&root_a));
    assert_eq!(status.project_count, 2);
    assert_eq!(status.budget_bytes, max_size_mb * 1024 * 1024);
    assert_eq!(
        status.total_bytes,
        roll_a.total_bytes + roll_b.total_bytes,
        "top-level total_bytes must sum every shard's rollup bytes"
    );
    assert!(status.total_bytes > 0);

    // Per-project entry counts are the O(1) rollup counts, not a scan.
    let shards = registry.list_shards();
    let info_a = shards
        .iter()
        .find(|shard| shard.normalized_root == normalize_project_root(&root_a))
        .expect("shard A should be listed");
    let info_b = shards
        .iter()
        .find(|shard| shard.normalized_root == normalize_project_root(&root_b))
        .expect("shard B should be listed");
    assert_eq!(info_a.entry_count, roll_a.entry_count);
    assert_eq!(info_b.entry_count, roll_b.entry_count);

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}
