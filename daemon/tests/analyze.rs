use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest};
use import_lens_daemon::pipeline::analyze::{AnalysisContext, analyze_import};
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
    let path = std::env::temp_dir().join(format!("import-lens-analyze-{suffix}"));
    fs::create_dir_all(&path).expect("temp workspace should be created");
    path
}

fn write_package(workspace: &Path, name: &str, package_json: &str, source: &str) {
    let package_root = workspace.join("node_modules").join(name);
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(package_root.join("package.json"), package_json)
        .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), source).expect("package entry should be written");
}

#[test]
fn analyze_import_computes_static_sizes_for_local_package_entry() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "tiny-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "export const used = 1;\nexport const unused = 2;\n",
    );
    let active_document_path = workspace.join("src").join("index.ts");
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path,
    };
    let request = ImportRequest {
        specifier: "tiny-lib".to_owned(),
        package_name: "tiny-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(result.minified_bytes > 0);
    assert!(result.gzip_bytes > 0);
    assert_eq!(result.side_effects, false);
    assert_eq!(result.is_cjs, false);
}

#[test]
fn analyze_import_returns_partial_error_result_on_missing_entry() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "broken-lib",
        r#"{"version":"1.0.0","module":"missing.js","sideEffects":true}"#,
        "export const value = 1;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "broken-lib".to_owned(),
        package_name: "broken-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(
        result
            .error
            .expect("missing entry should produce an error")
            .contains("entry")
    );
    assert_eq!(result.raw_bytes, 0);
    assert_eq!(result.minified_bytes, 0);
    assert_eq!(result.gzip_bytes, 0);
}
