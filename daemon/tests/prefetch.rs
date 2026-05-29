use import_lens_daemon::{
    ipc::protocol::ImportKind,
    prefetch::{CancellationToken, package_json_dependency_names, package_json_prewarm_requests},
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn temp_workspace() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("import-lens-prefetch-{suffix}"));
    fs::create_dir_all(&path).expect("temp workspace should be created");
    path
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

#[test]
fn package_json_dependency_names_include_dependencies_and_dev_dependencies() {
    let names = package_json_dependency_names(
        r#"{
            "dependencies": {
                "react": "^19"
            },
            "devDependencies": {
                "lodash-es": "^4"
            }
        }"#,
    )
    .expect("package json should parse");

    assert_eq!(names, vec!["lodash-es".to_owned(), "react".to_owned()]);
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
fn package_json_prewarm_requests_use_installed_package_versions() {
    let workspace = temp_workspace();
    let package_json_path = workspace.join("package.json");
    let active_document_path = workspace.join("package.json");
    write_installed_package(&workspace, "react", "19.2.3");
    fs::write(
        &package_json_path,
        r#"{"dependencies":{"react":"^19.0.0"}}"#,
    )
    .expect("workspace package json should be written");

    let requests = package_json_prewarm_requests(&package_json_path, &active_document_path)
        .expect("prewarm requests should be created");

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].specifier, "react");
    assert_eq!(requests[0].version, "19.2.3");
    assert_eq!(requests[0].import_kind, ImportKind::Default);
    assert_eq!(requests[1].specifier, "react");
    assert_eq!(requests[1].version, "19.2.3");
    assert_eq!(requests[1].import_kind, ImportKind::Namespace);
}

#[test]
fn cancellation_token_invalidates_existing_jobs() {
    let token = CancellationToken::default();
    let generation = token.generation();

    assert!(token.is_current(generation));

    token.cancel();

    assert!(!token.is_current(generation));
}
