use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::analyze::{AnalysisContext, analyze_import};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Once,
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

static FIXTURES_EXTRACTED: Once = Once::new();

fn extract_fixture_archives() {
    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");

    let archive = fixtures_dir.join("packages.zip");
    let target = fixtures_dir.join("packages");

    if archive.exists() && !target.exists() {
        let file = fs::File::open(&archive).expect("fixture archive should be readable");
        let mut zip = zip::ZipArchive::new(file).expect("fixture archive should be a valid zip");
        zip.extract(&target)
            .expect("fixture archive should extract successfully");
    }
}

fn fixture_workspace(name: &str) -> PathBuf {
    FIXTURES_EXTRACTED.call_once(extract_fixture_archives);
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("packages")
        .join(name)
}

fn import_request(
    specifier: &str,
    package_name: &str,
    version: &str,
    import_kind: ImportKind,
    named: &[&str],
) -> ImportRequest {
    ImportRequest {
        specifier: specifier.to_owned(),
        package_name: package_name.to_owned(),
        version: version.to_owned(),
        named: named.iter().map(|name| (*name).to_owned()).collect(),
        import_kind,
        runtime: ImportRuntime::Component,
    }
}

fn fixture_context(fixture: &Path) -> AnalysisContext {
    AnalysisContext {
        workspace_root: fixture.to_path_buf(),
        active_document_path: fixture.join("src").join("app.ts"),
    }
}

fn assert_named_import_is_smaller_than_namespace_import(
    fixture_name: &str,
    package_name: &str,
    version: &str,
    named_export: &str,
) {
    let fixture = fixture_workspace(fixture_name);
    let context = fixture_context(&fixture);

    let named = analyze_import(
        &context,
        &import_request(
            package_name,
            package_name,
            version,
            ImportKind::Named,
            &[named_export],
        ),
    );
    let namespace = analyze_import(
        &context,
        &import_request(
            package_name,
            package_name,
            version,
            ImportKind::Namespace,
            &[],
        ),
    );

    assert_eq!(named.error, None);
    assert_eq!(namespace.error, None);
    assert!(named.brotli_bytes > 0);
    assert!(namespace.brotli_bytes > 0);
    assert!(
        named.brotli_bytes < namespace.brotli_bytes,
        "named import should be smaller than namespace import: named={named:?}, namespace={namespace:?}",
    );
}

#[test]
fn analyze_lodash_named_import_is_smaller_than_namespace_import() {
    assert_named_import_is_smaller_than_namespace_import(
        "lodash-es@4.17.21",
        "lodash-es",
        "4.17.21",
        "debounce",
    );
}

#[test]
fn analyze_date_fns_named_import_is_smaller_than_namespace_import() {
    assert_named_import_is_smaller_than_namespace_import(
        "date-fns@4.1.0",
        "date-fns",
        "4.1.0",
        "format",
    );
}

#[test]
fn analyze_uuid_named_import_is_smaller_than_namespace_import() {
    assert_named_import_is_smaller_than_namespace_import("uuid@13.0.0", "uuid", "13.0.0", "v4");
}

#[test]
fn analyze_react_default_import_is_conservative_commonjs() {
    let fixture = fixture_workspace("react@19.2.3");
    let context = fixture_context(&fixture);
    let result = analyze_import(
        &context,
        &import_request("react", "react", "19.2.3", ImportKind::Default, &[]),
    );

    assert_eq!(result.error, None);
    assert!(result.brotli_bytes > 0);
    assert!(result.side_effects);
    assert!(!result.truly_treeshakeable);
    assert!(
        result.is_cjs,
        "react default entry should be reported as conservative CommonJS: {result:?}",
    );
}

#[test]
fn analyze_zod_namespace_import_measures_full_module_entry() {
    let fixture = fixture_workspace("zod@4.1.13");
    let context = fixture_context(&fixture);
    let result = analyze_import(
        &context,
        &import_request("zod", "zod", "4.1.13", ImportKind::Namespace, &[]),
    );

    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(result.brotli_bytes > 0);
    assert!(!result.truly_treeshakeable);
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
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(result.minified_bytes > 0);
    assert!(result.gzip_bytes > 0);
    assert!(!result.side_effects);
    assert!(!result.is_cjs);
    assert!(
        result
            .module_breakdown
            .as_ref()
            .is_some_and(|breakdown| breakdown.iter().any(|item| item.path.contains("tiny-lib"))),
        "{result:?}",
    );
}

#[test]
fn analyze_import_uses_full_graph_when_side_effects_true() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "effectful-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
        "export const used = 1;\nexport const unused = heavy();\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let named = analyze_import(
        &context,
        &import_request(
            "effectful-lib",
            "effectful-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );
    let namespace = analyze_import(
        &context,
        &import_request(
            "effectful-lib",
            "effectful-lib",
            "1.0.0",
            ImportKind::Namespace,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(named.error, None);
    assert_eq!(namespace.error, None);
    assert_eq!(named.raw_bytes, namespace.raw_bytes);
    assert!(named.side_effects);
    assert!(!named.truly_treeshakeable);
}

#[test]
fn analyze_import_uses_full_graph_when_side_effects_missing() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "implicit-effects-lib",
        r#"{"version":"1.0.0","module":"index.js"}"#,
        "export const used = 1;\nexport const unused = heavy();\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let named = analyze_import(
        &context,
        &import_request(
            "implicit-effects-lib",
            "implicit-effects-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );
    let namespace = analyze_import(
        &context,
        &import_request(
            "implicit-effects-lib",
            "implicit-effects-lib",
            "1.0.0",
            ImportKind::Namespace,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(named.error, None);
    assert_eq!(namespace.error, None);
    assert_eq!(named.raw_bytes, namespace.raw_bytes);
    assert!(named.side_effects);
    assert!(!named.truly_treeshakeable);
}

#[test]
fn analyze_import_reports_side_effect_array_conservatively() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "array-effects-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.css"]}"#,
        "export const used = 1;\nexport const unused = heavy();\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let named = analyze_import(
        &context,
        &import_request(
            "array-effects-lib",
            "array-effects-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );
    let namespace = analyze_import(
        &context,
        &import_request(
            "array-effects-lib",
            "array-effects-lib",
            "1.0.0",
            ImportKind::Namespace,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(named.error, None);
    assert_eq!(namespace.error, None);
    assert_eq!(named.raw_bytes, namespace.raw_bytes);
    assert!(named.side_effects);
    assert!(!named.truly_treeshakeable);
    assert!(
        named
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "side_effects"
                && diagnostic.message.contains("array")),
        "{named:?}",
    );
}

#[test]
fn analyze_import_reports_missing_named_export_without_failing_result() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "missing-export-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "export const present = 1;\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "missing-export-lib",
            "missing-export-lib",
            "1.0.0",
            ImportKind::Named,
            &["missing"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "exports"
                && diagnostic.message.contains("missing")),
        "{result:?}",
    );
}

#[test]
fn analyze_import_reports_missing_esm_default_export_without_failing_result() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "missing-default-esm-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "export const present = 1;\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "missing-default-esm-lib",
            "missing-default-esm-lib",
            "1.0.0",
            ImportKind::Default,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "exports"
                && diagnostic.message.contains("default")),
        "{result:?}",
    );
}

#[test]
fn analyze_import_reports_missing_cjs_default_export_without_failing_result() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "missing-default-cjs-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "missing-default-cjs-lib",
        "index.cjs",
        "exports.present = 1;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "missing-default-cjs-lib",
            "missing-default-cjs-lib",
            "1.0.0",
            ImportKind::Default,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.is_cjs);
    assert!(result.raw_bytes > 0);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "exports"
                && diagnostic.message.contains("default")),
        "{result:?}",
    );
}

#[test]
fn analyze_namespace_import_reports_oxc_fallback_diagnostic() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "fallback-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "export const value = ;\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "fallback-lib",
            "fallback-lib",
            "1.0.0",
            ImportKind::Namespace,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "oxc_fallback"
                && diagnostic.message.contains("static entry")),
        "{result:?}",
    );
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&repo).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(result.gzip_bytes > 0);
}

#[test]
fn analyze_commonjs_literal_require_graph_includes_required_modules() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "cjs-graph-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "cjs-graph-lib",
        "index.cjs",
        "const helper = require('./helper.cjs');\nexports.used = helper.used;",
    );
    write_package_file(
        &workspace,
        "cjs-graph-lib",
        "helper.cjs",
        "exports.used = 'required helper payload';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "cjs-graph-lib".to_owned(),
        package_name: "cjs-graph-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.is_cjs);
    assert!(result.raw_bytes > 0);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("helper.cjs"))
        }),
        "{result:?}",
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "cjs_fallback"),
        "{result:?}",
    );
}

#[test]
fn analyze_commonjs_literal_require_resolves_directory_package_manifest() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "cjs-dir-require-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "cjs-dir-require-lib",
        "index.cjs",
        "const client = require('./createClient');\nexports.used = client.used;",
    );
    write_package_file(
        &workspace,
        "cjs-dir-require-lib",
        "createClient/package.json",
        r#"{"name":"create-client-fixture","main":"./node.cjs"}"#,
    );
    write_package_file(
        &workspace,
        "cjs-dir-require-lib",
        "createClient/node.cjs",
        "exports.used = 'directory manifest payload';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "cjs-dir-require-lib".to_owned(),
        package_name: "cjs-dir-require-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.is_cjs);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules.iter().any(|module| {
                module.path.contains("createClient") && module.path.ends_with("node.cjs")
            })
        }),
        "{result:?}",
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "cjs_resolution"),
        "{result:?}",
    );
}

#[test]
fn analyze_commonjs_dynamic_require_uses_static_fallback_diagnostic() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "dynamic-cjs-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "dynamic-cjs-lib",
        "index.cjs",
        "const name = './helper.cjs';\nconst helper = require(name);\nexports.used = helper.used;",
    );
    write_package_file(
        &workspace,
        "dynamic-cjs-lib",
        "helper.cjs",
        "exports.used = 'dynamic helper payload';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "dynamic-cjs-lib".to_owned(),
        package_name: "dynamic-cjs-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.is_cjs);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "cjs_fallback"),
        "{result:?}",
    );
}

#[test]
fn analyze_commonjs_module_exports_object_reports_named_exports() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "object-cjs-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "object-cjs-lib",
        "index.cjs",
        "const value = 1;\nmodule.exports = { value, alias: value };",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "object-cjs-lib".to_owned(),
        package_name: "object-cjs-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["alias".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.is_cjs);
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "exports"),
        "{result:?}",
    );
}

#[test]
fn analyze_import_reports_circular_dependency_diagnostics() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "cycle-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "import { child } from './child.js';\nexport const value = child;",
    );
    write_package_file(
        &workspace,
        "cycle-lib",
        "child.js",
        "import { value } from './index.js';\nexport const child = value || 1;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "cycle-lib".to_owned(),
        package_name: "cycle-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "circular_dependency"),
        "{result:?}",
    );
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(!result.is_cjs);
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
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(!result.is_cjs);
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
        runtime: ImportRuntime::Component,
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
                    "import": { "default": "./browser/utils.mjs" },
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
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
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(!result.is_cjs);
}

#[test]
fn analyze_import_transforms_typescript_jsx_and_type_only_modules() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "ts-pkg",
        r#"{"version":"1.0.0","module":"src/index.ts","sideEffects":false}"#,
        "// js entry",
    );
    write_package_file(
        &workspace,
        "ts-pkg",
        "src/index.ts",
        "import type { Label } from './types';\nimport { componentValue } from './component';\nimport { legacyValue } from './legacy.jsx';\nexport type { Label } from './types';\nexport const x: number = componentValue + legacyValue;",
    );
    write_package_file(
        &workspace,
        "ts-pkg",
        "src/types.ts",
        "export type Label = { text: string };",
    );
    write_package_file(
        &workspace,
        "ts-pkg",
        "src/component.tsx",
        "export const componentValue: number = <span data-value=\"1\" /> ? 1 : 0;",
    );
    write_package_file(
        &workspace,
        "ts-pkg",
        "src/legacy.jsx",
        "export const legacyValue = <div /> ? 2 : 0;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "ts-pkg".to_owned(),
        package_name: "ts-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["x".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(!result.is_cjs);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("component.tsx"))
                && modules
                    .iter()
                    .any(|module| module.path.ends_with("legacy.jsx"))
                && !modules
                    .iter()
                    .any(|module| module.path.ends_with("types.ts"))
        }),
        "{result:?}",
    );
}

#[test]
fn analyze_import_includes_json_import_as_synthetic_js() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "json-pkg",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "import data, { answer } from './data.json';\nexport const value = data.answer + answer;",
    );
    write_package_file(
        &workspace,
        "json-pkg",
        "data.json",
        r#"{"answer":21,"label":"ok"}"#,
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "json-pkg".to_owned(),
        package_name: "json-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("data.json") && module.bytes > 0)
        }),
        "{result:?}",
    );
}

#[test]
fn analyze_import_keeps_assets_external_with_diagnostics() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "asset-pkg",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "import './style.css';\nimport logo from './logo.svg';\nexport const value = logo;",
    );
    write_package_file(
        &workspace,
        "asset-pkg",
        "style.css",
        ".root { color: red; }",
    );
    write_package_file(
        &workspace,
        "asset-pkg",
        "logo.svg",
        r#"<svg viewBox="0 0 1 1"></svg>"#,
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "asset-pkg".to_owned(),
        package_name: "asset-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.stage == "asset")
            .count()
            >= 2,
        "{result:?}",
    );
    assert!(
        !result.module_breakdown.as_ref().is_some_and(|modules| {
            modules.iter().any(|module| {
                module.path.ends_with("style.css") || module.path.ends_with("logo.svg")
            })
        }),
        "{result:?}",
    );
}

#[test]
fn analyze_import_strips_comments_for_minified_estimate() {
    let workspace = temp_workspace();
    let source = r#"
        /*
         * Huge copyright banner
         * with lots of text to increase bytes
         */
        export const a = 1; // Inline comment
        const b = "/* not a comment */";
    "#;

    write_package(
        &workspace,
        "comment-pkg",
        r#"{"version":"1.0.0","main":"index.js"}"#,
        source,
    );

    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "comment-pkg".to_owned(),
        package_name: "comment-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(result.error.is_none());

    let expected_stripped = r#"export const a = 1; const b = "/* not a comment */";"#;
    assert_eq!(result.minified_bytes, expected_stripped.len() as u64);
}

#[test]
fn analyze_import_rejects_files_over_size_limit() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "huge-pkg",
        r#"{"version":"1.0.0","main":"index.js"}"#,
        "// this will be replaced",
    );

    // Create a 6MB file
    let path = workspace
        .join("node_modules")
        .join("huge-pkg")
        .join("index.js");
    let data = vec![b'a'; 6 * 1024 * 1024];
    fs::write(&path, &data).expect("huge file should be written");

    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "huge-pkg".to_owned(),
        package_name: "huge-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Default,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(result.error.is_some());
    assert!(result.error.as_ref().unwrap().contains("exceeds 5MB limit"));
}
