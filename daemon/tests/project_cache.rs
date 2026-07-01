use import_lens_daemon::{
    cache::project::{
        ProjectCacheRegistry, normalize_project_root, project_cache_shard_id,
        remove_legacy_central_cache,
    },
    ipc::protocol::{ConfidenceLevel, ImportResult},
};
use std::{fs, path::PathBuf};

mod common;

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
