use import_lens_daemon::{
    cache::key::{cache_key_for_resolved_import, decode_cache_identity},
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::resolver::{ResolvedPackage, SideEffectsMode},
};
use serde_json::json;
use std::path::{Path, PathBuf};

fn request(import_kind: ImportKind, named: &[&str], runtime: ImportRuntime) -> ImportRequest {
    ImportRequest {
        specifier: "pkg".to_owned(),
        package_name: "pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: named.iter().map(|name| (*name).to_owned()).collect(),
        import_kind,
        runtime,
    }
}

fn resolved(root: &Path) -> ResolvedPackage {
    ResolvedPackage {
        package_root: root.join("node_modules").join("pkg"),
        package_json: json!({"version":"1.0.0","module":"index.js"}),
        entry_path: root.join("node_modules").join("pkg").join("index.js"),
        is_cjs: false,
        side_effects: SideEffectsMode::False,
    }
}

#[test]
fn cache_key_v3_includes_analyzer_revision() {
    let key = cache_key_for_resolved_import(
        &request(ImportKind::Named, &["used"], ImportRuntime::Component),
        &resolved(&PathBuf::from("C:/workspace-a")),
    );

    let identity = decode_cache_identity(&key).expect("v3 key should decode");
    assert!(
        identity.analyzer_version.ends_with("+graph2"),
        "analyzer version should invalidate graph2 accuracy changes: {identity:?}",
    );
}

#[test]
fn cache_key_v3_distinguishes_named_dynamic_from_dynamic_import() {
    let root = PathBuf::from("C:/workspace-a");
    let resolved = resolved(&root);
    let named_dynamic = cache_key_for_resolved_import(
        &request(ImportKind::Named, &["dynamic"], ImportRuntime::Component),
        &resolved,
    );
    let dynamic = cache_key_for_resolved_import(
        &request(ImportKind::Dynamic, &[], ImportRuntime::Component),
        &resolved,
    );

    assert!(named_dynamic.starts_with("v3:"));
    assert!(dynamic.starts_with("v3:"));
    assert_ne!(named_dynamic, dynamic);

    let identity = decode_cache_identity(&named_dynamic).expect("v3 key should decode");
    assert_eq!(identity.import_kind, ImportKind::Named);
    assert_eq!(identity.named_exports, vec!["dynamic".to_owned()]);
}

#[test]
fn cache_key_v3_separates_workspaces_for_same_package_version() {
    let request = request(ImportKind::Namespace, &[], ImportRuntime::Component);
    let left = cache_key_for_resolved_import(&request, &resolved(&PathBuf::from("C:/workspace-a")));
    let right =
        cache_key_for_resolved_import(&request, &resolved(&PathBuf::from("C:/workspace-b")));

    assert_ne!(left, right);
}

#[test]
fn cache_key_v3_separates_runtime_profiles() {
    let root = PathBuf::from("C:/workspace-a");
    let resolved = resolved(&root);
    let component = cache_key_for_resolved_import(
        &request(ImportKind::Namespace, &[], ImportRuntime::Component),
        &resolved,
    );
    let server = cache_key_for_resolved_import(
        &request(ImportKind::Namespace, &[], ImportRuntime::Server),
        &resolved,
    );

    assert_ne!(component, server);
}
