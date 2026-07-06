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
fn cache_key_includes_analyzer_revision() {
    let key = cache_key_for_resolved_import(
        &request(ImportKind::Named, &["used"], ImportRuntime::Component),
        &resolved(&PathBuf::from("C:/workspace-a")),
    );

    let identity = decode_cache_identity(&key).expect("key should decode");
    assert!(
        identity.analyzer_version.ends_with("+graph2"),
        "analyzer version should invalidate graph2 accuracy changes: {identity:?}",
    );
}

#[test]
fn cache_key_distinguishes_named_dynamic_from_dynamic_import() {
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

    assert!(named_dynamic.starts_with("v4:"));
    assert!(dynamic.starts_with("v4:"));
    assert_ne!(named_dynamic, dynamic);

    let identity = decode_cache_identity(&named_dynamic).expect("key should decode");
    assert_eq!(identity.import_kind, ImportKind::Named);
    assert_eq!(identity.named_exports, vec!["dynamic".to_owned()]);
}

#[test]
fn cache_key_separates_workspaces_for_same_package_version() {
    let request = request(ImportKind::Namespace, &[], ImportRuntime::Component);
    let left = cache_key_for_resolved_import(&request, &resolved(&PathBuf::from("C:/workspace-a")));
    let right =
        cache_key_for_resolved_import(&request, &resolved(&PathBuf::from("C:/workspace-b")));

    assert_ne!(left, right);
}

#[test]
fn cache_key_matches_any_package_decodes_once_and_tests_set_membership() {
    use import_lens_daemon::cache::key::cache_key_matches_any_package;
    use std::collections::HashSet;

    let key = cache_key_for_resolved_import(
        &request(ImportKind::Namespace, &[], ImportRuntime::Component),
        &resolved(&PathBuf::from("C:/workspace-a")),
    );

    assert!(cache_key_matches_any_package(
        &key,
        &HashSet::from(["pkg".to_owned(), "other".to_owned()])
    ));
    assert!(!cache_key_matches_any_package(
        &key,
        &HashSet::from(["other".to_owned(), "third".to_owned()])
    ));

    // Legacy (non-versioned) keys fall back to plaintext prefix matching.
    assert!(cache_key_matches_any_package(
        "react@18.3.1::default",
        &HashSet::from(["react".to_owned()])
    ));
    assert!(!cache_key_matches_any_package(
        "react@18.3.1::default",
        &HashSet::from(["vue".to_owned()])
    ));
}

#[test]
fn cache_key_separates_runtime_profiles() {
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

#[test]
fn path_is_definitely_gone_only_on_notfound() {
    use import_lens_daemon::cache::key::path_is_definitely_gone;
    let missing = std::env::temp_dir().join("il-surely-absent-xyz-123");
    let _ = std::fs::remove_file(&missing); // ensure absent
    assert!(path_is_definitely_gone(&missing)); // NotFound → gone
    assert!(!path_is_definitely_gone(std::env::temp_dir().as_path())); // exists → keep
}
