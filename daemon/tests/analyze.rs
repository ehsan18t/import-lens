use import_lens_daemon::ipc::protocol::{
    ConfidenceLevel, ImportKind, ImportRequest, ImportRuntime,
};
use import_lens_daemon::pipeline::analyze::{AnalysisContext, analyze_import};
use std::{
    fs,
    path::{Path, PathBuf},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-analyze")
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

fn fixture_workspace(name: &str) -> PathBuf {
    common::fixture_workspace(name)
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
    assert_eq!(result.confidence, ConfidenceLevel::High);
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
fn analyze_import_treats_unmatched_side_effect_array_as_treeshakeable() {
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
    assert!(named.raw_bytes < namespace.raw_bytes);
    assert!(!named.side_effects);
    assert!(named.truly_treeshakeable);
    assert!(
        named
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.stage != "side_effects"),
        "{named:?}",
    );
}

#[test]
fn analyze_import_includes_graph_modules_matching_side_effect_array_patterns() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "array-graph-effects-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["**/{polyfill,compat}/**/*.js"]}"#,
        "export const used = 1;\nexport { setup } from './dist/polyfill/browser/setup.js';\n",
    );
    write_package_file(
        &workspace,
        "array-graph-effects-lib",
        "dist/polyfill/browser/setup.js",
        "globalThis.__importLensSideEffect = 'kept';\nexport const setup = 'polyfill';\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "array-graph-effects-lib",
            "array-graph-effects-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.side_effects, "{result:?}");
    assert!(!result.truly_treeshakeable, "{result:?}");
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules.iter().any(|module| {
                module
                    .path
                    .replace('\\', "/")
                    .ends_with("dist/polyfill/browser/setup.js")
            })
        }),
        "{result:?}",
    );
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.stage == "side_effects"
                && diagnostic.details.iter().any(|detail| {
                    detail
                        .replace('\\', "/")
                        .contains("dist/polyfill/browser/setup.js")
                })
        }),
        "{result:?}",
    );
}

#[test]
fn analyze_dynamic_import_measures_full_module_graph() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "dynamic-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "export const used = 1;\nexport { heavy as unused } from './payload.js';\n",
    );
    write_package_file(
        &workspace,
        "dynamic-lib",
        "payload.js",
        &format!("export const heavy = '{}';", "x".repeat(2048)),
    );
    write_package_file(
        &workspace,
        "dynamic-lib",
        "index.js",
        "import { heavy } from './payload.js';\nexport const used = 1;\nexport const unused = heavy;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let named = analyze_import(
        &context,
        &import_request(
            "dynamic-lib",
            "dynamic-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );
    let dynamic = analyze_import(
        &context,
        &import_request(
            "dynamic-lib",
            "dynamic-lib",
            "1.0.0",
            ImportKind::Dynamic,
            &[],
        ),
    );
    let namespace = analyze_import(
        &context,
        &import_request(
            "dynamic-lib",
            "dynamic-lib",
            "1.0.0",
            ImportKind::Namespace,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(named.error, None);
    assert_eq!(dynamic.error, None);
    assert_eq!(namespace.error, None);
    assert_eq!(dynamic.raw_bytes, namespace.raw_bytes);
    assert!(
        dynamic.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("payload.js"))
        }),
        "{dynamic:?}",
    );
    assert!(!dynamic.truly_treeshakeable);
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
fn analyze_invalid_package_json_returns_approximate_directory_size() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "invalid-manifest-lib",
        "{ invalid json",
        "export const value = 1;",
    );
    write_package_file(
        &workspace,
        "invalid-manifest-lib",
        "node_modules/ignored/index.js",
        &"x".repeat(2048),
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "invalid-manifest-lib".to_owned(),
        package_name: "invalid-manifest-lib".to_owned(),
        version: "unknown".to_owned(),
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert_eq!(result.confidence, ConfidenceLevel::Low);
    assert!(
        result
            .confidence_reasons
            .iter()
            .any(|reason| reason.contains("approximate")),
        "{result:?}",
    );
    assert!(result.raw_bytes > 0, "{result:?}");
    assert!(result.raw_bytes < 2048, "{result:?}");
    assert_eq!(result.minified_bytes, result.raw_bytes);
    assert_eq!(result.brotli_bytes, result.raw_bytes);
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.stage == "manifest_fallback" && diagnostic.message.contains("(approx)")
        }),
        "{result:?}",
    );
}

#[test]
fn analyze_versionless_package_json_returns_approximate_directory_size() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "versionless-lib",
        r#"{"module":"index.js","sideEffects":false}"#,
        "export const value = 1;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "versionless-lib".to_owned(),
        package_name: "versionless-lib".to_owned(),
        version: "unknown".to_owned(),
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0, "{result:?}");
    assert_eq!(result.gzip_bytes, result.raw_bytes);
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            diagnostic.stage == "manifest_fallback" && diagnostic.message.contains("version")
        }),
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
fn analyze_named_import_falls_back_to_static_entry_after_oxc_failure() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "named-fallback-lib",
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
            "named-fallback-lib",
            "named-fallback-lib",
            "1.0.0",
            ImportKind::Named,
            &["value"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.raw_bytes > 0);
    assert_eq!(result.confidence, ConfidenceLevel::Low);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "oxc_fallback"
                && diagnostic.message.contains("static entry")),
        "{result:?}",
    );
    assert!(!result.truly_treeshakeable);
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
fn analyze_declaration_only_package_returns_zero_runtime_cost() {
    let workspace = temp_workspace();
    write_package_file(
        &workspace,
        "@types/demo",
        "package.json",
        r#"{"version":"1.0.0","types":"index.d.ts"}"#,
    );
    write_package_file(
        &workspace,
        "@types/demo",
        "index.d.ts",
        "export interface Demo { value: string }",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "@types/demo".to_owned(),
        package_name: "@types/demo".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["Demo".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(result.raw_bytes, 0);
    assert_eq!(result.minified_bytes, 0);
    assert_eq!(result.gzip_bytes, 0);
    assert_eq!(result.brotli_bytes, 0);
    assert_eq!(result.zstd_bytes, 0);
    assert!(!result.side_effects);
    assert!(!result.is_cjs);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "types_only"
                && diagnostic.message.contains("zero runtime")),
        "{result:?}",
    );
}

#[test]
fn analyze_declaration_only_detection_requires_declaration_files() {
    let workspace = temp_workspace();
    write_package_file(
        &workspace,
        "empty-runtime-lib",
        "package.json",
        r#"{"version":"1.0.0","main":"missing.js"}"#,
    );
    write_package_file(
        &workspace,
        "empty-runtime-lib",
        "README.md",
        "This package has no runtime entry and no declarations.",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "empty-runtime-lib".to_owned(),
        package_name: "empty-runtime-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Default,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(
        result
            .error
            .as_ref()
            .expect("missing runtime entry without declarations should still fail")
            .contains("entry"),
        "{result:?}",
    );
    assert_eq!(result.diagnostics[0].stage, "entry_resolution");
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
fn analyze_commonjs_graph_keeps_complete_internal_contribution_data() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "cjs-wide-graph-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );

    let mut entry = String::new();
    for index in 0..12 {
        entry.push_str(&format!("require('./dep-{index}.cjs');\n"));
        write_package_file(
            &workspace,
            "cjs-wide-graph-lib",
            &format!("dep-{index}.cjs"),
            &format!("exports.dep{index} = '{}';", "x".repeat(40)),
        );
    }
    entry.push_str("exports.used = 1;");
    write_package_file(&workspace, "cjs-wide-graph-lib", "index.cjs", &entry);

    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "cjs-wide-graph-lib".to_owned(),
        package_name: "cjs-wide-graph-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(result.is_cjs);
    assert_eq!(
        result.module_breakdown.as_ref().map(Vec::len),
        Some(10),
        "{result:?}",
    );
    assert_eq!(result.internal_contributions.len(), 13, "{result:?}");
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
fn analyze_commonjs_bracket_exports_include_required_modules() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "bracket-cjs-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "bracket-cjs-lib",
        "index.cjs",
        "const helper = require('./helper.cjs');\nexports[\"used\"] = helper.used;",
    );
    write_package_file(
        &workspace,
        "bracket-cjs-lib",
        "helper.cjs",
        "exports.used = 'required helper payload';",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "bracket-cjs-lib",
            "bracket-cjs-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("helper.cjs"))
        }),
        "{result:?}",
    );
    assert!(
        result
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.stage != "cjs_fallback"),
        "{result:?}",
    );
}

#[test]
fn analyze_commonjs_ignores_require_inside_regex_literal() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "regex-cjs-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "regex-cjs-lib",
        "index.cjs",
        "const pattern = /require('.\\/heavy.cjs')/;\nexports.used = pattern.test('');",
    );
    write_package_file(
        &workspace,
        "regex-cjs-lib",
        "heavy.cjs",
        &format!("exports.heavy = '{}';", "x".repeat(4096)),
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "regex-cjs-lib",
            "regex-cjs-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(
        !result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("heavy.cjs"))
        }),
        "{result:?}",
    );
}

#[test]
fn analyze_commonjs_scans_require_inside_template_expressions_only() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "template-cjs-lib",
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
        "// unused js entry",
    );
    write_package_file(
        &workspace,
        "template-cjs-lib",
        "index.cjs",
        "const text = `ignore require('./heavy.cjs') but include ${require('./helper.cjs').used}`;\nexports.used = text;",
    );
    write_package_file(
        &workspace,
        "template-cjs-lib",
        "helper.cjs",
        "exports.used = 'template helper payload';",
    );
    write_package_file(
        &workspace,
        "template-cjs-lib",
        "heavy.cjs",
        &format!("exports.heavy = '{}';", "x".repeat(4096)),
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "template-cjs-lib",
            "template-cjs-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("helper.cjs"))
        }),
        "{result:?}",
    );
    assert!(
        !result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("heavy.cjs"))
        }),
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
fn analyze_import_resolves_typescript_source_via_js_extension_alias() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "alias-ts-pkg",
        r#"{"version":"1.0.0","module":"src/index.ts","sideEffects":false}"#,
        "// js entry",
    );
    write_package_file(
        &workspace,
        "alias-ts-pkg",
        "src/index.ts",
        "import { helper } from './helper.js';\nexport const value = helper;",
    );
    write_package_file(
        &workspace,
        "alias-ts-pkg",
        "src/helper.ts",
        "export const helper: number = 1;",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "alias-ts-pkg".to_owned(),
        package_name: "alias-ts-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(
        result.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.ends_with("helper.ts"))
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
fn analyze_import_falls_back_for_oversized_entries() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "huge-pkg",
        r#"{"version":"1.0.0","main":"index.js"}"#,
        "// this will be replaced",
    );

    let path = workspace
        .join("node_modules")
        .join("huge-pkg")
        .join("index.js");
    let data = vec![b'a'; 25 * 1024 * 1024];
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
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(result.confidence, ConfidenceLevel::Low);
    assert!(result.brotli_bytes > 0, "{result:?}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "oversized_entry"),
        "{result:?}",
    );
}

#[test]
fn analyze_import_analyzes_typescript_scale_entries() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "large-pkg",
        r#"{"version":"1.0.0","main":"index.js"}"#,
        "// this will be replaced",
    );

    let path = workspace
        .join("node_modules")
        .join("large-pkg")
        .join("index.js");
    let data = vec![b'a'; 9 * 1024 * 1024];
    fs::write(&path, &data).expect("large file should be written");

    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("main.ts"),
    };
    let request = ImportRequest {
        specifier: "large-pkg".to_owned(),
        package_name: "large-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![],
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(result.error.is_none(), "{result:?}");
    assert!(result.brotli_bytes > 0, "{result:?}");
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "oversized_entry"),
        "{result:?}",
    );
}
