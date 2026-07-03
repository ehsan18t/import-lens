use import_lens_daemon::{
    cache::{
        key::fingerprints_for_paths,
        project::{
            ProjectCacheRegistry, normalize_project_root, project_cache_shard_id,
            remove_legacy_central_cache,
        },
    },
    ipc::protocol::{ConfidenceLevel, ImportResult},
};
use redb::{Database, ReadableDatabase, TableDefinition};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

mod common;

const CACHE_DB_FILE_NAME: &str = "importlens.redb";
const CACHE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("size_cache");
const SHARD_METADATA_FILE_NAME: &str = "importlens-project-cache.json";

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
        diagnostics: Vec::new(),
        module_breakdown: None,
        shared_bytes: None,
        internal_contributions: Vec::new(),
    }
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
fn project_cache_registry_stores_projects_in_separate_shards() {
    let storage = common::temp_workspace("import-lens-project-cache");
    let first_root = storage.join("first-app");
    let second_root = storage.join("second-app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);

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
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);
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
        let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);
        let cache = registry.cache_for_root(&project_root);
        cache.insert_with_fingerprints(
            "vue@3.4.0::default".to_owned(),
            result("vue"),
            fingerprints_for_paths([stale_dependency.clone()]),
        );
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
    }

    fs::remove_file(&stale_dependency).expect("stale dependency fixture should be removed");

    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);
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
        let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);
        let cache = registry.cache_for_root(&project_root);
        cache.insert("react@18.3.1::default".to_owned(), result("react"));
        cache.insert("vue@3.4.0::default".to_owned(), result("vue"));
        cache.insert("lodash@4.17.21::default".to_owned(), result("lodash"));
    }

    // Fresh registry: the shard is on disk (unloaded), exercising the batched
    // disk-shard invalidation path.
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);
    registry.invalidate_packages(&["react".to_owned(), "vue".to_owned()]);

    let cache_path = storage.join(project_cache_shard_id(&project_root));
    let db_path = cache_path.join(CACHE_DB_FILE_NAME);
    assert!(!disk_cache_entry_exists(&db_path, "react@18.3.1::default"));
    assert!(!disk_cache_entry_exists(&db_path, "vue@3.4.0::default"));
    assert!(disk_cache_entry_exists(&db_path, "lodash@4.17.21::default"));

    fs::remove_dir_all(storage).expect("temp storage should be removed");
}

#[test]
fn project_cache_registry_removes_current_project_without_removing_other_shards() {
    let storage = common::temp_workspace("import-lens-project-cache-remove");
    let first_root = storage.join("first-app");
    let second_root = storage.join("second-app");
    let registry = ProjectCacheRegistry::new(Some(storage.clone()), true, 512, 30);

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
