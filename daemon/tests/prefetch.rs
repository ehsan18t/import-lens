use import_lens_daemon::{
    cache::key::{CacheIdentity, decode_cache_identity},
    ipc::protocol::ImportKind,
    prefetch::{CancellationToken, Prefetcher, package_json_dependency_names, prewarm_pool},
    service::ImportLensService,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-prefetch")
}

fn write_installed_package(workspace: &Path, package_name: &str, version: &str) {
    let package_root = workspace.join("node_modules").join(package_name);
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        format!(r#"{{"version":"{version}","module":"index.js","sideEffects":false}}"#),
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "export default 1; export const value = 1;",
    )
    .expect("package entry should be written");
}

fn write_installed_named_only_package(workspace: &Path, package_name: &str, version: &str) {
    let package_root = workspace.join("node_modules").join(package_name);
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        format!(r#"{{"version":"{version}","module":"index.js","sideEffects":false}}"#),
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "export const value = 1; export const other = 2;",
    )
    .expect("package entry should be written");
}

/// Prewarm the workspace's `package.json` through the production entry point and return the
/// identities the project cache holds once at least `expected` entries have landed.
fn prewarmed_identities(workspace: &Path, expected: usize) -> Vec<CacheIdentity> {
    let storage = temp_workspace();
    let service = Arc::new(ImportLensService::new_with_cache_policy(
        Some(storage.clone()),
        true,
        512,
        32,
    ));
    let prefetcher = Prefetcher::new();
    let package_json_path = workspace.join("package.json");
    prefetcher.prewarm_package_json(
        Arc::clone(&service),
        package_json_path.clone(),
        package_json_path,
    );

    let cached_keys = || {
        service.flush_cache().expect("flush should succeed");
        service.recent_cache_keys(workspace, 8)
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while cached_keys().len() < expected {
        assert!(
            Instant::now() < deadline,
            "the prewarm should cache {expected} entries"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Give a job that should NOT have been queued the time to land, so its absence means something.
    std::thread::sleep(Duration::from_millis(500));
    let identities = cached_keys()
        .iter()
        .map(|key| decode_cache_identity(key).expect("a cache key should decode"))
        .collect();

    drop(prefetcher);
    drop(service);
    let _ = fs::remove_dir_all(&storage);
    identities
}

#[test]
fn package_json_dependency_names_include_all_installable_dependency_sections() {
    let names = package_json_dependency_names(
        r#"{
            "dependencies": {
                "react": "^19"
            },
            "devDependencies": {
                "lodash-es": "^4"
            },
            "peerDependencies": {
                "@types/react": "^19"
            },
            "optionalDependencies": {
                "fsevents": "^2"
            }
        }"#,
    )
    .expect("package json should parse");

    assert_eq!(
        names,
        vec![
            "@types/react".to_owned(),
            "fsevents".to_owned(),
            "lodash-es".to_owned(),
            "react".to_owned()
        ]
    );
}

#[test]
fn package_json_dependency_names_ignore_non_string_dependency_versions() {
    let names = package_json_dependency_names(
        r#"{
            "dependencies": {
                "react": "19.2.3",
                "bad": { "workspace": "*" }
            }
        }"#,
    )
    .expect("package json should parse");

    assert_eq!(names, vec!["react".to_owned()]);
}

#[test]
fn package_json_prewarm_caches_the_installed_version_as_default_and_namespace_imports() {
    let workspace = temp_workspace();
    write_installed_package(&workspace, "react", "19.2.3");
    fs::write(
        workspace.join("package.json"),
        r#"{"dependencies":{"react":"^19.0.0"}}"#,
    )
    .expect("workspace package json should be written");

    let mut identities = prewarmed_identities(&workspace, 2);

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    identities.sort_by_key(|identity| format!("{:?}", identity.import_kind));
    assert_eq!(identities.len(), 2, "{identities:?}");
    assert!(
        identities
            .iter()
            .all(|identity| identity.specifier == "react" && identity.package_version == "19.2.3"),
        "{identities:?}"
    );
    assert_eq!(identities[0].import_kind, ImportKind::Default);
    assert_eq!(identities[1].import_kind, ImportKind::Namespace);
}

#[test]
fn package_json_prewarm_skips_the_default_import_of_a_package_without_one() {
    let workspace = temp_workspace();
    write_installed_named_only_package(&workspace, "named-lib", "1.0.0");
    fs::write(
        workspace.join("package.json"),
        r#"{"dependencies":{"named-lib":"^1.0.0"}}"#,
    )
    .expect("workspace package json should be written");

    let identities = prewarmed_identities(&workspace, 1);

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(identities.len(), 1, "{identities:?}");
    assert_eq!(identities[0].specifier, "named-lib");
    assert_eq!(identities[0].import_kind, ImportKind::Namespace);
}

#[test]
fn cancellation_token_invalidates_existing_jobs() {
    let token = CancellationToken::default();
    let generation = token.generation();

    assert!(token.is_current(generation));

    token.cancel();

    assert!(!token.is_current(generation));
}

#[test]
fn prefetcher_drop_cancels_current_generation() {
    let prefetcher = Prefetcher::new();
    let cancellation = std::sync::Arc::clone(prefetcher.cancellation());
    let generation = cancellation.next_generation();

    assert!(cancellation.is_current(generation));

    drop(prefetcher);

    assert!(!cancellation.is_current(generation));
}

#[test]
fn prewarm_pool_reuses_one_fallible_thread_pool() {
    let first = prewarm_pool().expect("prewarm pool should build") as *const rayon::ThreadPool;
    let second = prewarm_pool().expect("prewarm pool should be reused") as *const rayon::ThreadPool;

    assert_eq!(first, second);
}
