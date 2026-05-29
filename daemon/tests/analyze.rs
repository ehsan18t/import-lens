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

fn write_package_file(workspace: &Path, package_name: &str, relative_path: &str, source: &str) {
    let path = workspace
        .join("node_modules")
        .join(package_name)
        .join(relative_path);
    fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
        .expect("fixture directory should be created");
    fs::write(path, source).expect("fixture file should be written");
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
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].stage, "entry_resolution");
    assert!(
        result.diagnostics[0]
            .details
            .iter()
            .any(|detail| detail.contains("candidate:"))
    );
}

#[test]
fn analyze_import_resolves_dotted_nestjs_style_subpath() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "@nestjs/common",
        r#"{"version":"11.1.24","sideEffects":true}"#,
        "exports.DynamicModule = class DynamicModule {};",
    );
    write_package_file(
        &workspace,
        "@nestjs/common",
        "interfaces/modules/dynamic-module.interface.js",
        "exports.DynamicModule = class DynamicModule {};",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("app.module.ts"),
    };
    let request = ImportRequest {
        specifier: "@nestjs/common/interfaces/modules/dynamic-module.interface".to_owned(),
        package_name: "@nestjs/common".to_owned(),
        version: "11.1.24".to_owned(),
        named: vec!["DynamicModule".to_owned()],
        import_kind: ImportKind::Named,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(result.gzip_bytes > 0);
}

#[test]
fn analyze_import_resolves_package_from_active_document_tree() {
    let repo = temp_workspace();
    let backend = repo.join("ensurily-backend");
    let frontend = repo.join("ensurily-frontend");
    fs::create_dir_all(&frontend).expect("sibling workspace should be created");
    write_package(
        &backend,
        "dayjs",
        r#"{"version":"1.11.13","sideEffects":false}"#,
        "module.exports = require('./dayjs.min');",
    );
    write_package_file(
        &backend,
        "dayjs",
        "plugin/utc.js",
        "module.exports = function utc() {};",
    );
    let context = AnalysisContext {
        workspace_root: frontend,
        active_document_path: backend.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "dayjs/plugin/utc".to_owned(),
        package_name: "dayjs".to_owned(),
        version: "1.11.13".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&repo).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(result.gzip_bytes > 0);
}

#[test]
fn analyze_import_rejects_unsafe_package_names() {
    let workspace = temp_workspace();
    fs::create_dir_all(workspace.join("outside")).expect("outside fixture should be created");
    fs::write(
        workspace.join("outside").join("package.json"),
        r#"{"version":"1.0.0","main":"index.js"}"#,
    )
    .expect("outside manifest should be written");
    fs::write(
        workspace.join("outside").join("index.js"),
        "module.exports = 1;",
    )
    .expect("outside entry should be written");
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "../outside".to_owned(),
        package_name: "../outside".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(
        result
            .error
            .expect("unsafe package name should produce an error")
            .contains("unsafe package name")
    );
}

#[test]
fn analyze_import_resolves_subpath_via_exports_map() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "svelte",
        r#"{
            "version": "5.0.0",
            "exports": {
                ".": { "import": "./src/index.js" },
                "./transition": { "import": "./src/transition/index.js", "default": "./src/transition/index-server.js" }
            }
        }"#,
        "export const noop = 1;",
    );
    write_package_file(
        &workspace,
        "svelte",
        "src/transition/index.js",
        "export function fade(node) { return {}; }",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("App.svelte"),
    };
    let request = ImportRequest {
        specifier: "svelte/transition".to_owned(),
        package_name: "svelte".to_owned(),
        version: "5.0.0".to_owned(),
        named: vec!["fade".to_owned()],
        import_kind: ImportKind::Named,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert_eq!(result.is_cjs, false);
}

#[test]
fn analyze_import_resolves_root_entry_via_exports_dot() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "modern-pkg",
        r#"{
            "version": "2.0.0",
            "main": "lib/cjs.js",
            "exports": {
                ".": { "import": "./esm/index.mjs", "require": "./lib/cjs.js" }
            }
        }"#,
        "// legacy entry",
    );
    write_package_file(
        &workspace,
        "modern-pkg",
        "esm/index.mjs",
        "export const value = 42;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "modern-pkg".to_owned(),
        package_name: "modern-pkg".to_owned(),
        version: "2.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert_eq!(result.is_cjs, false);
}

#[test]
fn analyze_import_resolves_string_shorthand_exports() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "simple-esm",
        r#"{"version": "1.0.0", "exports": "./dist/index.mjs"}"#,
        "// should not be used",
    );
    write_package_file(
        &workspace,
        "simple-esm",
        "dist/index.mjs",
        "export const greeting = 'hello';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("app.ts"),
    };
    let request = ImportRequest {
        specifier: "simple-esm".to_owned(),
        package_name: "simple-esm".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
}

#[test]
fn analyze_import_resolves_conditional_exports_with_nested_conditions() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "cond-pkg",
        r#"{
            "version": "3.0.0",
            "exports": {
                "./utils": {
                    "browser": { "import": "./browser/utils.mjs" },
                    "default": "./node/utils.js"
                }
            }
        }"#,
        "// root",
    );
    write_package_file(
        &workspace,
        "cond-pkg",
        "browser/utils.mjs",
        "export const platform = 'browser';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "cond-pkg/utils".to_owned(),
        package_name: "cond-pkg".to_owned(),
        version: "3.0.0".to_owned(),
        named: vec!["platform".to_owned()],
        import_kind: ImportKind::Named,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
}

#[test]
fn analyze_import_resolves_wildcard_exports_pattern() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "wildcard-pkg",
        r#"{
            "version": "1.0.0",
            "exports": {
                ".": "./index.js",
                "./*": "./dist/*.js"
            }
        }"#,
        "export const root = 1;",
    );
    write_package_file(
        &workspace,
        "wildcard-pkg",
        "dist/helpers.js",
        "export function help() {}",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "wildcard-pkg/helpers".to_owned(),
        package_name: "wildcard-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
}

#[test]
fn analyze_import_errors_on_unmapped_subpath_when_exports_present() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "strict-pkg",
        r#"{
            "version": "1.0.0",
            "exports": {
                ".": "./index.js",
                "./allowed": "./allowed.js"
            }
        }"#,
        "export const root = 1;",
    );
    write_package_file(
        &workspace,
        "strict-pkg",
        "internal/secret.js",
        "export const secret = 42;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "strict-pkg/internal/secret".to_owned(),
        package_name: "strict-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(result.error.is_some());
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .contains("not defined in the exports map")
    );
}

#[test]
fn analyze_import_resolves_array_fallback_exports() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "array-pkg",
        r#"{
            "version": "1.0.0",
            "exports": {
                ".": [{ "import": "./esm/index.mjs" }, "./fallback.js"]
            }
        }"#,
        "// fallback entry",
    );
    write_package_file(
        &workspace,
        "array-pkg",
        "esm/index.mjs",
        "export const value = 'array-resolved';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("app.ts"),
    };
    let request = ImportRequest {
        specifier: "array-pkg".to_owned(),
        package_name: "array-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
}

#[test]
fn analyze_import_resolves_top_level_condition_map_exports() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "condtop-pkg",
        r#"{
            "version": "1.0.0",
            "exports": { "import": "./esm/index.mjs", "require": "./cjs/index.cjs" }
        }"#,
        "// should not be used",
    );
    write_package_file(
        &workspace,
        "condtop-pkg",
        "esm/index.mjs",
        "export const topLevel = true;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "condtop-pkg".to_owned(),
        package_name: "condtop-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert_eq!(result.is_cjs, false);
}

#[test]
fn analyze_import_rejects_typescript_entry_files() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "ts-pkg",
        r#"{"version":"1.0.0","main":"src/index.ts"}"#,
        "// js entry",
    );
    write_package_file(
        &workspace,
        "ts-pkg",
        "src/index.ts",
        "export const x: number = 42;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "ts-pkg".to_owned(),
        package_name: "ts-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(result.error.is_some());
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .contains("TypeScript source file")
    );
}
