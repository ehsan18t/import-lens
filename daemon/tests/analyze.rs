use import_lens_daemon::ipc::protocol::{
    ConfidenceLevel, ImportKind, ImportRequest, ImportRuntime, MeasuredSizes,
};
use import_lens_daemon::pipeline::analyze::{AnalysisContext, analyze_import};
use import_lens_daemon::pipeline::resolver::{SideEffectsKind, resolve_package_entry};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-analyze")
}

fn write_package(workspace: &Path, name: &str, package_json: &str, source: &str) {
    write_package_at(
        &workspace.join("node_modules").join(name),
        package_json,
        "index.js",
        source,
    );
}

/// A package whose entry is not necessarily `index.js`, at a root that is not necessarily inside
/// `node_modules`.
///
/// Both degrees of freedom are the point. A `sideEffects` pattern is matched against the entry's
/// **package-relative** path, so an entry at `dist/index.js` is the only shape that exercises a
/// pattern carrying a `/` — and a package root outside `node_modules` is the everyday
/// workspace-linked (monorepo) layout, where `node_modules/<name>` is a junction and the entry's
/// real path has no `node_modules` component at all.
fn write_package_at(package_root: &Path, package_json: &str, entry: &str, source: &str) {
    let entry_path = package_root.join(entry);
    fs::create_dir_all(entry_path.parent().expect("entry should have a parent"))
        .expect("package root should be created");
    fs::write(package_root.join("package.json"), package_json)
        .expect("package manifest should be written");
    fs::write(entry_path, source).expect("package entry should be written");
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
    assert!(common::measured_sizes(&named).brotli_bytes > 0);
    assert!(common::measured_sizes(&namespace).brotli_bytes > 0);
    assert!(
        common::measured_sizes(&named).brotli_bytes
            < common::measured_sizes(&namespace).brotli_bytes,
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
    assert!(common::measured_sizes(&result).brotli_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
    assert!(common::measured_sizes(&result).brotli_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
    assert!(common::measured_sizes(&result).minified_bytes > 0);
    assert!(common::measured_sizes(&result).gzip_bytes > 0);
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
fn analyze_import_reports_declared_side_effects() {
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
    assert!(named.side_effects);
    assert!(namespace.side_effects);
    assert!(!named.truly_treeshakeable);
}

#[test]
fn analyze_import_reports_missing_side_effect_metadata_conservatively() {
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
    assert!(named.side_effects);
    assert!(namespace.side_effects);
    assert!(!named.truly_treeshakeable);
}

#[test]
fn analyze_named_import_excludes_dependency_used_only_by_unreachable_export() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "branchy-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "import { small } from './small.js';\nimport { huge } from './huge.js';\nexport const used = small;\nexport const unused = huge;\n",
    );
    write_package_file(
        &workspace,
        "branchy-lib",
        "small.js",
        "export const small = 'small payload';\n",
    );
    write_package_file(
        &workspace,
        "branchy-lib",
        "huge.js",
        &format!("export const huge = '{}';\n", "x".repeat(120_000)),
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let named = analyze_import(
        &context,
        &import_request(
            "branchy-lib",
            "branchy-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );
    let namespace = analyze_import(
        &context,
        &import_request(
            "branchy-lib",
            "branchy-lib",
            "1.0.0",
            ImportKind::Namespace,
            &[],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(named.error, None, "{named:?}");
    assert_eq!(namespace.error, None, "{namespace:?}");
    assert!(
        common::measured_sizes(&named).raw_bytes * 4 < common::measured_sizes(&namespace).raw_bytes,
        "named import should prune unused huge dependency: named={named:?}, namespace={namespace:?}",
    );
    assert!(
        !named.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.replace('\\', "/").ends_with("huge.js"))
        }),
        "{named:?}",
    );
    assert!(
        namespace.module_breakdown.as_ref().is_some_and(|modules| {
            modules
                .iter()
                .any(|module| module.path.replace('\\', "/").ends_with("huge.js"))
        }),
        "{namespace:?}",
    );
}

#[test]
fn analyze_truly_treeshakeable_uses_minified_size_ratio() {
    let workspace = temp_workspace();
    let removable_padding = format!("/* {} */", "x".repeat(200_000));
    let dense_unused_payload = "y".repeat(8_000);
    write_package(
        &workspace,
        "minified-ratio-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        &format!(
            "export const used = {removable_padding} 1;\nexport const unused = '{dense_unused_payload}';\n"
        ),
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "minified-ratio-lib",
            "minified-ratio-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None, "{result:?}");
    assert!(
        result.truly_treeshakeable,
        "tree-shakeability should compare minified sizes instead of raw source bytes: {result:?}",
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
    assert_eq!(
        common::measured_sizes(&dynamic).raw_bytes,
        common::measured_sizes(&namespace).raw_bytes
    );
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
fn analyze_import_reports_missing_named_export_as_zero_size_error() {
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
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("missing"))
    );
    assert_eq!(
        result.sizes(),
        None,
        "a missing export is Unmeasured: no size, not a zero"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "missing_export"
                && diagnostic.message.contains("missing")),
        "{result:?}",
    );
}

#[test]
fn analyze_import_reports_missing_esm_default_export_as_zero_size_error() {
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
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("default"))
    );
    assert_eq!(
        result.sizes(),
        None,
        "a missing export is Unmeasured: no size, not a zero"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "missing_export"
                && diagnostic.message.contains("default")),
        "{result:?}",
    );
}

/// The manifest fabricator, gone (ADR-0006 §1). An unreadable `package.json` used to be answered
/// with the package directory's bytes ON DISK — and that one number was written to all five size
/// fields, so this import's "brotli size" was an uncompressed directory that also counted its
/// tests, source maps and `node_modules`. It is Unmeasured now: no size at all, and a stage
/// (`package_manifest`) that says why.
#[test]
fn analyze_invalid_package_json_is_unmeasured_not_approximated_from_the_directory() {
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
    assert_eq!(result.sizes(), None, "{result:?}");
    assert_eq!(result.unmeasured_stage(), Some("package_manifest"));
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("manifest")),
        "{result:?}",
    );
    assert_eq!(result.confidence, ConfidenceLevel::Low);
}

#[test]
fn analyze_versionless_package_json_is_unmeasured() {
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
    assert_eq!(result.sizes(), None, "{result:?}");
    assert_eq!(result.unmeasured_stage(), Some("package_manifest"));
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("version")),
        "{result:?}",
    );
}

/// The engine fabricator, gone. A package that cannot be parsed used to be sized from its entry
/// file ALONE — the whole graph behind it uncounted — and served with `error: None`, which every
/// `!result.error` check in the system waves through. The failure keeps the stage it happened at
/// (§12); what it no longer keeps is a number.
#[test]
fn analyze_namespace_import_of_an_unparseable_package_is_unmeasured_under_its_stage() {
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
    assert_eq!(result.sizes(), None, "{result:?}");
    assert_eq!(result.unmeasured_stage(), Some("parse"));
    assert!(result.error.is_some(), "{result:?}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "parse"),
        "{result:?}",
    );
}

#[test]
fn analyze_named_import_of_an_unparseable_package_is_unmeasured() {
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
    assert_eq!(result.sizes(), None, "{result:?}");
    assert_eq!(result.unmeasured_stage(), Some("parse"));
    assert_eq!(result.confidence, ConfidenceLevel::Low);
    assert!(!result.truly_treeshakeable);
}

#[test]
fn analyze_invalid_semantic_module_is_unmeasured_at_the_minify_boundary() {
    // A semantically invalid module (here a duplicate `const`) is rejected at the minifier's
    // semantic pass, and Rolldown reports it as a parse failure. It used to be sized from the
    // entry file alone; there is no size now.
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "semantic-invalid-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "export const value = 1;\nconst duplicate = 1;\nconst duplicate = 2;\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "semantic-invalid-lib",
            "semantic-invalid-lib",
            "1.0.0",
            ImportKind::Named,
            &["value"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.sizes(), None, "{result:?}");
    assert!(result.error.is_some(), "{result:?}");
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
            .as_deref()
            .expect("missing entry should produce an error")
            .contains("entry")
    );
    assert_eq!(result.sizes(), None);
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
    assert_eq!(
        result.sizes(),
        Some(MeasuredSizes::ZERO),
        "a declarations-only package is MEASURED at zero, not Unmeasured: {result:?}",
    );
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
fn analyze_native_binary_only_package_is_measured_at_zero_and_labelled() {
    // A `bin`-only package whose real tool ships as a platform-specific native binary in
    // `optionalDependencies` (the Biome shape). It has no importable JS entry, so it is MEASURED at
    // zero and labelled `native_binary_only`, not shown as a bare "unavailable" (B3).
    let workspace = temp_workspace();
    write_package_file(
        &workspace,
        "native-cli",
        "package.json",
        r#"{"version":"1.0.0","bin":{"native-cli":"bin/native-cli"},"optionalDependencies":{"@scope/native-cli-win32-x64":"1.0.0","@scope/native-cli-linux-x64-musl":"1.0.0"}}"#,
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "native-cli".to_owned(),
        package_name: "native-cli".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Default,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(
        result.sizes(),
        Some(MeasuredSizes::ZERO),
        "a native-binary-only package is MEASURED at zero, not shown unavailable: {result:?}",
    );
    assert!(result.is_native_binary_only(), "{result:?}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "native_binary_only"),
        "{result:?}",
    );
}

#[test]
fn analyze_native_binary_backed_package_with_a_js_entry_keeps_its_size_and_is_flagged() {
    // A native-binary-backed package whose JS entry DOES resolve — a thin shim (the TypeScript 7
    // version stub shape). Its measured JS size stands, with a `native_binary` flag beside it, so
    // the number is not read as the whole cost (B3).
    let workspace = temp_workspace();
    write_package_file(
        &workspace,
        "shim-lib",
        "package.json",
        r#"{"version":"1.0.0","main":"index.js","optionalDependencies":{"@scope/shim-lib-win32-x64":"1.0.0"}}"#,
    );
    write_package_file(
        &workspace,
        "shim-lib",
        "index.js",
        "export const version = \"1.2.3\";\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "shim-lib".to_owned(),
        package_name: "shim-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None, "{result:?}");
    assert!(
        result.sizes().is_some_and(|sizes| sizes.raw_bytes > 0),
        "the resolved JS shim keeps its measured size: {result:?}",
    );
    assert!(
        result.is_native_binary(),
        "a native-backed package with a JS entry must carry the native-binary flag: {result:?}",
    );
    assert!(!result.is_native_binary_only(), "{result:?}");
}

#[test]
fn analyze_native_backed_package_with_a_broken_declared_entry_stays_unavailable() {
    // The package DECLARES a JS entry (`main`) that does not exist — a broken or partial install —
    // and also lists a platform native optional dep. It must NOT be flattened to a confident zero;
    // it stays Unmeasured at `entry_resolution`, the honest answer for something we could not
    // measure (B3 review finding: the native-binary-only zero requires no DECLARED entry).
    let workspace = temp_workspace();
    write_package_file(
        &workspace,
        "broken-native",
        "package.json",
        r#"{"version":"1.0.0","main":"missing.js","optionalDependencies":{"@scope/broken-native-win32-x64":"1.0.0"}}"#,
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "broken-native".to_owned(),
        package_name: "broken-native".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Default,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(
        result.sizes(),
        None,
        "a package with a broken declared entry must stay Unmeasured, not be zeroed: {result:?}",
    );
    assert!(!result.is_native_binary_only(), "{result:?}");
    assert_eq!(
        result.diagnostics[0].stage, "entry_resolution",
        "{result:?}"
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
    assert!(common::measured_sizes(&result).gzip_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
    assert!(common::measured_sizes(&result).gzip_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
        named: vec!["greeting".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
        named: vec!["help".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
        named: vec!["value".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
        named: vec!["topLevel".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };

    let result = analyze_import(&context, &request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(result.error, None);
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
    assert!(common::measured_sizes(&result).raw_bytes > 0, "{result:?}");
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
    assert!(common::measured_sizes(&result).raw_bytes > 0);
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
    // The entry-alone fabricator, gone. An entry over the module source limit used to be sized
    // from that ONE file with the whole graph behind it uncounted, and served as the import's
    // size. It is Unmeasured — deterministically so, which is why it is still cached.
    assert_eq!(result.sizes(), None, "{result:?}");
    assert_eq!(result.unmeasured_stage(), Some("oversized_entry"));
    assert_eq!(result.confidence, ConfidenceLevel::Low);
    assert!(result.error.is_some(), "{result:?}");
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
    assert!(
        common::measured_sizes(&result).brotli_bytes > 0,
        "{result:?}"
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "oversized_entry"),
        "{result:?}",
    );
}

/// §12's failure table requires each failure to surface under the stage it happened
/// at. Collapsing every fallback-eligible failure into one `engine_fallback` label
/// erased the distinction between a parse error, a resolve error, a graph-limit
/// breach and an OXC validation failure, leaving the real stage recoverable only by
/// reading the message. This guard fails if that label is reintroduced.
#[test]
fn a_fallback_diagnostic_never_collapses_the_failure_stage() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "stage-lib",
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
            "stage-lib",
            "stage-lib",
            "1.0.0",
            ImportKind::Named,
            &["value"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");

    assert!(
        !result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "engine_fallback"),
        "the failure stage must be preserved, not replaced with a generic label: {result:?}"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "parse"),
        "a source syntax error must surface under the `parse` stage: {result:?}"
    );
}

/// **Every form `package.json#sideEffects` is really written in, pinned against what Rolldown
/// ACTUALLY RETAINED.**
///
/// `side_effects` is a property of THE IMPORT (§7.4 / FR-021): is *the entry being measured* one
/// the package declares effectful? Rolldown asks that same question of that same manifest — the
/// plugin hands it the package-root `package.json` for the entry it pre-resolves — and its answer
/// decides real bytes. [ADR-0002]: where we read the metadata upstream reads, **our answer must be
/// upstream's answer**. A badge that disagrees with what Rolldown kept is a wrong badge, whichever
/// direction it errs in: "conservative" does not redeem a badge that contradicts the very build the
/// reported size came out of.
///
/// So no row here asserts a badge on its own: every row asserts the badge, **what Rolldown really
/// kept**, and the badge cascade (`truly_treeshakeable`, confidence) together. Assert the badge
/// alone and `[]` passes for the wrong reason; assert retention alone and nothing pins the badge.
///
/// **The probe is exact, and it is not the obvious one.** Rolldown sweeps every side-effectful
/// statement of an *included* module in unconditionally — so a payload in an entry whose export is
/// imported is retained under **every** declaration, `false` included (measured: all fourteen rows
/// kept it, 60084 B each), and proves nothing. The one gap it leaves is
/// `tree_shaking::on_demand`: a statement that evaluates effects **and reads a module-level
/// binding** is *gated* for a module Rolldown determined `UserDefined(false)`, and joins only when
/// that module's **body is demanded** — a used **own** export, or its namespace. A **pure
/// re-export demands nothing.**
///
/// So each fixture's entry is a BARREL: it re-exports its whole surface from `impl.js` and carries
/// one gated statement of its own (`globalThis.… = payload`, 60 KB). Nothing else can keep those
/// bytes. Their survival IS Rolldown's `check_side_effects_for(entry)` — the identical question the
/// badge answers, asked of the identical manifest, and answered in bytes.
///
/// It is a PROPERTY over the forms, not a pile of one-offs: a new declaration form cannot be
/// handled without being classified here.
///
/// Two rows are answered by Rolldown from the AST rather than from the manifest — an **absent**
/// field, and a value that is not a bool/string/array. Their retention is a fact about the entry's
/// source, not about the declaration, and metadata cannot answer what only source analysis can: the
/// daemon is conservative there by spec (FR-021, absent ⇒ `true`), and §7.4 forbids it the AST
/// purity check that would decide otherwise. Both fixtures carry a real top-level effect, so the
/// conservative answer is also the retained one.
#[test]
fn every_side_effects_form_answers_with_what_rolldown_retained() {
    /// A top-level effect big enough that its presence in the measured bytes is unmistakable.
    const EFFECT_PAYLOAD_BYTES: usize = 60_000;

    struct Form {
        /// The `sideEffects` value, verbatim JSON — `None` when the field is absent altogether.
        declaration: Option<&'static str>,
        /// Where the entry sits **relative to the package root**, because that is the string the
        /// pattern is matched against — Rolldown's and ours.
        ///
        /// It is not decoration. Every row here used to hard-code `index.js`, so every pattern in
        /// the table either carried **no separator** (`index.js`, `*.{js,ts}`) or began with `**/`
        /// — and both of those land in the `**/`-prefixed branch of the matcher, which matched the
        /// *junk absolute path* Rolldown was being handed **by accident**. The one branch that
        /// cannot: a pattern that **contains a `/`**, which the matcher uses VERBATIM and anchors
        /// at the package root. Not one row reached it, so a 3.7x undercount on `refractor` sat
        /// under a green suite.
        entry: &'static str,
        /// Whether the entry being measured is one this declaration makes effectful.
        entry_is_effectful: bool,
        why: &'static str,
    }

    let forms = [
        Form {
            declaration: Some("false"),
            entry: "index.js",
            entry_is_effectful: false,
            why: "the package declares itself pure",
        },
        Form {
            declaration: Some("true"),
            entry: "index.js",
            entry_is_effectful: true,
            why: "the package declares itself effectful",
        },
        Form {
            declaration: None,
            entry: "index.js",
            entry_is_effectful: true,
            why: "absent: nothing declared, so conservative (FR-021) — Rolldown analyses the source",
        },
        Form {
            declaration: Some(r#""index.js""#),
            entry: "index.js",
            entry_is_effectful: true,
            why: "the string form is one glob, and it names the entry",
        },
        Form {
            declaration: Some(r#""**/*.css""#),
            entry: "index.js",
            entry_is_effectful: false,
            why: "the string form is one glob, and it says nothing about a JavaScript entry",
        },
        Form {
            declaration: Some(r#"["index.js"]"#),
            entry: "index.js",
            entry_is_effectful: true,
            why: "an array glob matching the entry",
        },
        Form {
            declaration: Some(r#"["**/*.css"]"#),
            entry: "index.js",
            entry_is_effectful: false,
            why: "the everyday declaration: it says nothing about a JavaScript entry",
        },
        Form {
            declaration: Some("[]"),
            entry: "index.js",
            entry_is_effectful: false,
            why: "an empty pattern list matches nothing, so nothing in the package is effectful",
        },
        Form {
            declaration: Some(r#"["**/*"]"#),
            entry: "index.js",
            entry_is_effectful: true,
            why: "an array glob that matches everything matches the entry too",
        },
        Form {
            declaration: Some(r#"["*.{js,ts}"]"#),
            entry: "index.js",
            entry_is_effectful: true,
            why: "a brace pattern, expanded by the matcher itself, matches the entry",
        },
        Form {
            declaration: Some(r#"["index.js",42]"#),
            entry: "index.js",
            entry_is_effectful: true,
            why: "a non-string element is dropped by the parse; the pattern that remains matches",
        },
        Form {
            declaration: Some(r#"["**/*.css",42]"#),
            entry: "index.js",
            entry_is_effectful: false,
            why: "a non-string element is dropped by the parse; the pattern that remains misses",
        },
        Form {
            declaration: Some("[42]"),
            entry: "index.js",
            entry_is_effectful: false,
            why: "every element dropped by the parse: an empty pattern list, which matches nothing",
        },
        Form {
            declaration: Some(r#"{"index.js":true}"#),
            entry: "index.js",
            entry_is_effectful: true,
            why: "not a bool, string or array: unreadable as a declaration, so conservative",
        },
        // ---- Patterns that carry a `/`: matched VERBATIM, anchored at the package root. ----
        // The branch nothing above can reach, and the one every real package uses.
        Form {
            declaration: Some(r#"["dist/index.js"]"#),
            entry: "dist/index.js",
            entry_is_effectful: true,
            why: "an anchored pattern that names the entry: the package says its entry is effectful",
        },
        Form {
            declaration: Some(r#"["lib/all.js","lib/common.js"]"#),
            entry: "lib/common.js",
            entry_is_effectful: true,
            why: "refractor's literal declaration and entry: 83 KB of gated `register()` calls hung \
                  on this row matching",
        },
        Form {
            declaration: Some(r#"["src/index.js"]"#),
            entry: "dist/index.js",
            entry_is_effectful: false,
            why: "an anchored pattern that names a DIFFERENT file: it says nothing about the entry, \
                  and must not be made to match by a fix that merely un-anchors everything",
        },
        Form {
            declaration: Some(r#"["dist/*.js"]"#),
            entry: "dist/index.js",
            entry_is_effectful: true,
            why: "an anchored wildcard, and `*` does not cross a separator: it still names the entry",
        },
    ];

    let workspace = temp_workspace();
    let effect_payload = "z".repeat(EFFECT_PAYLOAD_BYTES);
    let unused_export_payload = "y".repeat(8_000);
    let mut observed: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut classified: HashSet<SideEffectsKind> = HashSet::new();

    for (index, form) in forms.iter().enumerate() {
        let package_name = format!("side-effects-form-{index}");
        let declared = form
            .declaration
            .map(|value| format!(r#","sideEffects":{value}"#))
            .unwrap_or_default();
        let entry = form.entry;
        let implementation = match entry.rsplit_once('/') {
            Some((directory, _)) => format!("{directory}/impl.js"),
            None => "impl.js".to_owned(),
        };
        // A barrel entry: its surface is a PURE RE-EXPORT (no body demand), and the one statement
        // it owns is gated (it evaluates an effect and reads the module-level `payload`). Inline
        // the payload into the statement and it stops referencing a binding, stops being gated,
        // and is swept in under every declaration — which is exactly what makes this fixture
        // discriminate and the obvious one not.
        write_package_at(
            &workspace.join("node_modules").join(&package_name),
            &format!(r#"{{"version":"1.0.0","module":"{entry}"{declared}}}"#),
            entry,
            &format!(
                "const payload = '{effect_payload}';\n\
                 globalThis.__il_side_effect_payload = payload;\n\
                 export {{ used, unused }} from './impl.js';\n"
            ),
        );
        write_package_file(
            &workspace,
            &package_name,
            &implementation,
            &format!("export const used = 1;\nexport const unused = '{unused_export_payload}';\n"),
        );

        let context = AnalysisContext {
            workspace_root: workspace.clone(),
            active_document_path: workspace.join("src").join("index.ts"),
        };
        let request = import_request(
            &package_name,
            &package_name,
            "1.0.0",
            ImportKind::Named,
            &["used"],
        );
        // Which ARM of `SideEffectsMode` this row exercises — the property, below, is that every
        // arm is exercised by some row.
        classified.insert(
            resolve_package_entry(&context.active_document_path, &request)
                .expect("the fixture package should resolve")
                .side_effects
                .kind(),
        );
        let result = analyze_import(&context, &request);
        let sizes = common::measured_sizes(&result);

        // ROLLDOWN'S OWN ANSWER, IN BYTES. The entry's top-level effect is unreachable from `used`,
        // so it survives the build if and only if Rolldown decided this entry is side-effectful.
        let retained = sizes.minified_bytes as usize >= EFFECT_PAYLOAD_BYTES;
        let kept_or_dropped = if retained { "KEPT" } else { "DROPPED" };
        let declaration = form.declaration.unwrap_or("<absent>");

        observed.push(format!(
            "entry={entry:<15} {declaration:<31} rolldown_retained={retained:<5} badge={:<5} \
             minified={:<7} treeshakeable={:<5} {:?} — {}",
            result.side_effects,
            sizes.minified_bytes,
            result.truly_treeshakeable,
            result.confidence,
            form.why,
        ));

        if result.error.is_some() {
            failures.push(format!("{declaration}: build failed: {:?}", result.error));
            continue;
        }
        if retained != form.entry_is_effectful {
            failures.push(format!(
                "{declaration}: ROLLDOWN, THE AUTHORITY, disagrees with this table — it \
                 {kept_or_dropped} the entry's top-level effect. The table is what is wrong."
            ));
        }
        if result.side_effects != form.entry_is_effectful {
            failures.push(format!(
                "{declaration}: side_effects={} while Rolldown {kept_or_dropped} the entry's \
                 effect. The badge must be Rolldown's own answer to the same question about the \
                 same manifest [ADR-0002] — {}",
                result.side_effects, form.why,
            ));
        }
        if form.entry_is_effectful {
            if result.truly_treeshakeable {
                failures.push(format!(
                    "{declaration}: an effectful entry can never be certified tree-shaken away"
                ));
            }
        } else {
            if !result.truly_treeshakeable {
                failures.push(format!(
                    "{declaration}: a declaration that does not describe the entry must not gate \
                     the full-package comparison off — `truly_treeshakeable: false` would then be \
                     true BY CONSTRUCTION"
                ));
            }
            if result.confidence != ConfidenceLevel::High {
                failures.push(format!(
                    "{declaration}: nothing is unmeasured and no glob is unmatched, so nothing \
                     here is conservative: confidence={:?} reasons={:?} diagnostics={:?}",
                    result.confidence, result.confidence_reasons, result.diagnostics,
                ));
            }
        }
    }

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");

    // The evidence, on demand (`cargo test … -- --nocapture`): what Rolldown really kept for every
    // form, beside the badge we printed over it. Captured and silent on a green run.
    println!("{}", observed.join("\n"));

    // THE PROPERTY, and the reason this is not fourteen one-offs: the table quantifies over the
    // arms of `SideEffectsMode` (emitted with the enum itself — see `side_effects_modes!`), so a
    // new declaration form cannot be classified without a row that pins it against what Rolldown
    // really retained. This used to be *claimed* and not enforced: a `Some(Value::Null)` arm could
    // be added to `side_effects_mode` and the whole suite stayed green.
    let unclassified = SideEffectsKind::ALL
        .iter()
        .filter(|kind| !classified.contains(kind))
        .collect::<Vec<_>>();
    assert!(
        unclassified.is_empty(),
        "every arm of `SideEffectsMode` must be exercised by a row here, against what Rolldown \
         really retained for it. No row produces: {unclassified:?}",
    );

    assert!(
        failures.is_empty(),
        "the badge must be the answer Rolldown gave the same manifest.\n\nMEASURED:\n{}\n\nFAILURES:\n{}",
        observed.join("\n"),
        failures.join("\n"),
    );
}

/// The link a package manager creates for a **workspace-internal** package: `node_modules/<name>`
/// is a junction (Windows) / symlink (POSIX) onto `packages/<name>`. Every pnpm, npm and yarn
/// workspace has one per internal package, and `fs::canonicalize` resolves it — so the entry's real
/// path contains **no `node_modules` component at all**.
fn link_package_directory(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        // A junction, not a symlink: `mklink /J` needs no privilege, `symlink_dir` does.
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("mklink should run");
        assert!(status.success(), "junction should be created: {link:?}");
    }
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(target, link).expect("symlink should be created");
}

/// **A workspace-linked package must get the same badge Rolldown's retention gives it.**
///
/// This is the monorepo layout, not an exotic one: `node_modules/<name>` is a junction onto
/// `packages/<name>`, and the entry's canonical path therefore has **no `node_modules` component**.
/// The daemon derived the entry's package-relative path by *scanning for a `node_modules`
/// component*, found none, and fell to `Unknown` — which reports **side-effectful**, forces
/// `truly_treeshakeable: false` BY CONSTRUCTION (the full-package comparison is gated on
/// `!side_effects` and never runs) and caps confidence at Medium, for **every monorepo-internal
/// package**, under **every** declaration form including `[]`.
///
/// Rolldown, meanwhile, DROPPED the entry's gated effect: `["**/*.css"]` says nothing about a
/// JavaScript entry. The badge contradicted the build its own number came out of. The package root
/// is carried right beside the entry path — stripping it is what the relative path always was.
#[test]
fn a_workspace_linked_package_answers_with_what_rolldown_retained() {
    const EFFECT_PAYLOAD_BYTES: usize = 60_000;

    let workspace = temp_workspace();
    let package_root = workspace.join("packages").join("linked-lib");
    let effect_payload = "z".repeat(EFFECT_PAYLOAD_BYTES);

    write_package_at(
        &package_root,
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["**/*.css"]}"#,
        "index.js",
        &format!(
            "const payload = '{effect_payload}';\n\
             globalThis.__il_side_effect_payload = payload;\n\
             export {{ used }} from './impl.js';\n"
        ),
    );
    fs::write(package_root.join("impl.js"), "export const used = 1;\n").expect("impl");
    fs::create_dir_all(workspace.join("node_modules")).expect("node_modules");
    link_package_directory(
        &package_root,
        &workspace.join("node_modules").join("linked-lib"),
    );

    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let result = analyze_import(
        &context,
        &import_request(
            "linked-lib",
            "linked-lib",
            "1.0.0",
            ImportKind::Named,
            &["used"],
        ),
    );
    let sizes = common::measured_sizes(&result);

    fs::remove_dir_all(&workspace).ok();

    assert_eq!(result.error, None, "{result:?}");
    // Rolldown's own answer, in bytes: `["**/*.css"]` does not describe a JavaScript entry, so the
    // entry is pure and its gated 60 KB effect is unreachable from `used`.
    let retained = sizes.minified_bytes as usize >= EFFECT_PAYLOAD_BYTES;
    assert!(
        !retained,
        "test setup: Rolldown must have dropped the gated effect for `[\"**/*.css\"]` — \
         minified={} {result:?}",
        sizes.minified_bytes,
    );
    assert!(
        !result.side_effects,
        "the badge must be Rolldown's own answer to the same question about the same manifest \
         [ADR-0002]. A `node_modules` scan cannot find the package-relative path of a package whose \
         real path has no `node_modules` component — but the package ROOT is carried right beside \
         the entry: {result:?}",
    );
    assert!(
        result.truly_treeshakeable,
        "a declaration that does not describe the entry must not gate the full-package comparison \
         off — `truly_treeshakeable: false` would then be true BY CONSTRUCTION for every \
         monorepo-internal package: {result:?}",
    );
    assert_eq!(
        result.confidence,
        ConfidenceLevel::High,
        "nothing is unmeasured and no glob is unmatched, so nothing here is conservative: \
         reasons={:?} diagnostics={:?}",
        result.confidence_reasons,
        result.diagnostics,
    );
}

/// A CSS-shipping package builds, and says what it did not count.
///
/// Rolldown 1.1.5 cannot bundle CSS at all: a `.css` module reaching it fails the WHOLE build at the
/// LINK stage (`UNSUPPORTED_FEATURE: Bundling CSS is no longer supported`) — it does not become an
/// emitted asset, and no output-shape guard is involved. So every package that ships a stylesheet
/// its entry imports (swiper, react-datepicker, react-toastify, most UI kits) was unmeasurable.
/// Nobody noticed, because the pipeline caught the failure and fabricated a size for it; delete the
/// fabricator without `plugin.rs` linking the stylesheet as an empty module and they all go BLANK.
///
/// The JS chunk is real and is measured exactly. The stylesheet's bytes are not in that number and
/// do ship with the package, so they are disclosed — and they cost the result its High confidence,
/// by design (see `engine::diagnostic_stage::UNCOUNTED_ASSETS`): a size that omits bytes the user's
/// bundle will really carry is not a High-confidence measurement of that package's cost.
#[test]
fn analyze_a_css_shipping_package_measures_its_javascript_and_discloses_the_rest() {
    let workspace = temp_workspace();
    write_package(
        &workspace,
        "styled-lib",
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        "import './styles.css';\nexport const widget = () => 'widget';\n",
    );
    write_package_file(
        &workspace,
        "styled-lib",
        "styles.css",
        ".widget { color: rebeccapurple; display: flex; }\n",
    );
    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };

    let result = analyze_import(
        &context,
        &import_request(
            "styled-lib",
            "styled-lib",
            "1.0.0",
            ImportKind::Named,
            &["widget"],
        ),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(
        result.error, None,
        "an emitted stylesheet is not an output-shape failure: {result:?}",
    );
    let sizes = common::measured_sizes(&result);
    assert!(sizes.brotli_bytes > 0, "{result:?}");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"
                && diagnostic.message.contains("styles.css")),
        "the bytes this size does NOT include must be named: {result:?}",
    );
    assert_eq!(
        result.confidence,
        ConfidenceLevel::Medium,
        "an asset-emitting package is Medium by design; High would claim a completeness the number \
         does not have: {result:?}",
    );
}
