use import_lens_daemon::{
    ipc::protocol::{
        BatchRequest, EnumerateExportsRequest, FileSizeRequest, ImportKind, ImportRequest,
        ImportRuntime, PROTOCOL_VERSION,
    },
    pipeline::graph::{build_module_graph_cached, clear_module_graph_cache},
    service::{ImportLensService, protocol_error_batch_response, protocol_error_exports_response},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-service")
}

fn write_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("tiny-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
}

fn write_tiny_package_with_source(workspace: &Path, source: &str) {
    let package_root = workspace.join("node_modules").join("tiny-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), source).expect("entry should be written");
}

fn write_runtime_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("runtime-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","exports":{"browser":"./browser.js","node":"./node.js","default":"./browser.js"},"sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("browser.js"), "export const value = 'b';")
        .expect("browser entry should be written");
    fs::write(
        package_root.join("node.js"),
        "export const value = 'node branch with different bytes';",
    )
    .expect("node entry should be written");
}

fn write_missing_export_effectful_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("missing-effectful-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const present = 1;")
        .expect("entry should be written");
}

fn write_dependent_package(workspace: &Path, helper_source: &str) {
    let package_root = workspace.join("node_modules").join("dependent-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import { helper } from './helper.js';\nexport const value = helper;",
    )
    .expect("entry should be written");
    fs::write(package_root.join("helper.js"), helper_source).expect("helper should be written");
}

fn write_parent_and_transitive_package(workspace: &Path, dependency_source: &str) {
    let parent_root = workspace.join("node_modules").join("parent-lib");
    fs::create_dir_all(&parent_root).expect("parent root should be created");
    fs::write(
        parent_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("parent manifest should be written");
    fs::write(
        parent_root.join("index.js"),
        "import { dep } from 'dep-lib';\nexport const value = dep;",
    )
    .expect("parent entry should be written");

    let dep_root = workspace.join("node_modules").join("dep-lib");
    fs::create_dir_all(&dep_root).expect("dep root should be created");
    fs::write(
        dep_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("dep manifest should be written");
    fs::write(dep_root.join("index.js"), dependency_source).expect("dep entry should be written");
}

fn write_export_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("exports-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "export const local = 1;\nexport { alpha as renamed } from './alpha.js';\nexport * from './more.js';",
    )
    .expect("entry should be written");
    fs::write(package_root.join("alpha.js"), "export const alpha = 1;")
        .expect("alpha module should be written");
    fs::write(
        package_root.join("more.js"),
        "export const beta = 2;\nexport default 3;",
    )
    .expect("more module should be written");
}

fn write_shared_packages(workspace: &Path) {
    let util_root = workspace.join("node_modules").join("shared-util");
    fs::create_dir_all(&util_root).expect("shared util root should be created");
    fs::write(
        util_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("shared util manifest should be written");
    fs::write(
        util_root.join("index.js"),
        "export const util = 'shared utility payload';",
    )
    .expect("shared util entry should be written");

    for package_name in ["left-lib", "right-lib"] {
        let package_root = workspace.join("node_modules").join(package_name);
        fs::create_dir_all(&package_root).expect("package root should be created");
        fs::write(
            package_root.join("package.json"),
            r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        )
        .expect("package manifest should be written");
        let export_name = package_name.replace("-lib", "").replace('-', "_");
        fs::write(
            package_root.join("index.js"),
            format!("import {{ util }} from 'shared-util';\nexport const {export_name} = util;"),
        )
        .expect("package entry should be written");
    }
}

fn write_shared_packages_with_many_unique_modules(workspace: &Path) {
    let util_root = workspace.join("node_modules").join("shared-small-util");
    fs::create_dir_all(&util_root).expect("shared util root should be created");
    fs::write(
        util_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("shared util manifest should be written");
    fs::write(util_root.join("index.js"), "export const util = 'u';")
        .expect("shared util entry should be written");

    for package_name in ["left-wide-lib", "right-wide-lib"] {
        let package_root = workspace.join("node_modules").join(package_name);
        fs::create_dir_all(&package_root).expect("package root should be created");
        fs::write(
            package_root.join("package.json"),
            r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        )
        .expect("package manifest should be written");

        let export_name = package_name.replace("-wide-lib", "").replace('-', "_");
        let mut entry = "import { util } from 'shared-small-util';\n".to_owned();
        for index in 0..11 {
            entry.push_str(&format!("import './local-{index}.js';\n"));
            fs::write(
                package_root.join(format!("local-{index}.js")),
                format!(
                    "globalThis.__importLensLocal{index} = '{}';",
                    "x".repeat(120)
                ),
            )
            .expect("local side-effect module should be written");
        }
        entry.push_str(&format!("export const {export_name} = util;"));
        fs::write(package_root.join("index.js"), entry).expect("package entry should be written");
    }
}

fn write_effectful_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("effectful-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "export const value = 1;\nexport const other = 2;",
    )
    .expect("entry should be written");
}

fn write_cjs_file_size_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("cjs-file-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","main":"index.cjs"}"#,
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.cjs"),
        "const helper = require('./helper.cjs');\nexports.value = helper.value;",
    )
    .expect("entry should be written");
    fs::write(
        package_root.join("helper.cjs"),
        "exports.value = 'cjs payload';",
    )
    .expect("helper should be written");
}

fn batch(workspace: &Path, request_id: u64) -> BatchRequest {
    BatchRequest {
        version: 1,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "tiny-lib".to_owned(),
            package_name: "tiny-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    }
}

fn effectful_batch(workspace: &Path, request_id: u64, import_kind: ImportKind) -> BatchRequest {
    BatchRequest {
        version: 1,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "effectful-lib".to_owned(),
            package_name: "effectful-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: if matches!(import_kind, ImportKind::Named) {
                vec!["value".to_owned()]
            } else {
                Vec::new()
            },
            import_kind,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    }
}

fn runtime_batch(workspace: &Path, request_id: u64, runtime: ImportRuntime) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "runtime-lib".to_owned(),
            package_name: "runtime-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime,
        }],
        streaming: false,
    }
}

fn missing_effectful_batch(
    workspace: &Path,
    request_id: u64,
    import_kind: ImportKind,
) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "missing-effectful-lib".to_owned(),
            package_name: "missing-effectful-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: if matches!(import_kind, ImportKind::Named) {
                vec!["missing".to_owned()]
            } else {
                Vec::new()
            },
            import_kind,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    }
}

fn package_batch(
    workspace: &Path,
    request_id: u64,
    package_name: &str,
    named: &str,
) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: package_name.to_owned(),
            package_name: package_name.to_owned(),
            version: "1.0.0".to_owned(),
            named: vec![named.to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    }
}

fn shared_batch(workspace: &Path, request_id: u64) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![
            ImportRequest {
                specifier: "left-lib".to_owned(),
                package_name: "left-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec!["left".to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            },
            ImportRequest {
                specifier: "right-lib".to_owned(),
                package_name: "right-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec!["right".to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            },
        ],
        streaming: false,
    }
}

fn wide_shared_batch(workspace: &Path, request_id: u64) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![
            ImportRequest {
                specifier: "left-wide-lib".to_owned(),
                package_name: "left-wide-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec!["left".to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            },
            ImportRequest {
                specifier: "right-wide-lib".to_owned(),
                package_name: "right-wide-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec!["right".to_owned()],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            },
        ],
        streaming: false,
    }
}

fn file_size_request(workspace: &Path, request_id: u64) -> FileSizeRequest {
    let batch = shared_batch(workspace, request_id);

    FileSizeRequest {
        message_type: "file_size".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: batch.workspace_root,
        active_document_path: batch.active_document_path,
        imports: batch.imports,
    }
}

fn cjs_file_size_request(workspace: &Path, request_id: u64) -> FileSizeRequest {
    FileSizeRequest {
        message_type: "file_size".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "cjs-file-lib".to_owned(),
            package_name: "cjs-file-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
    }
}

fn enumerate_exports_request(workspace: &Path, request_id: u64) -> EnumerateExportsRequest {
    EnumerateExportsRequest {
        message_type: "enumerate_exports".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("index.ts")
            .to_string_lossy()
            .to_string(),
        specifier: "exports-lib".to_owned(),
        package_name: "exports-lib".to_owned(),
        package_version: "1.0.0".to_owned(),
    }
}

#[test]
fn service_processes_batch_and_serves_second_request_from_cache() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let first = service.handle_batch(batch(&workspace, 7));
    let second = service.handle_batch(batch(&workspace, 8));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(first.request_id, 7);
    assert!(!first.imports[0].cache_hit);
    assert_eq!(second.request_id, 8);
    assert!(second.imports[0].cache_hit);
}

#[test]
fn service_does_not_reuse_same_package_version_across_workspaces() {
    let left_workspace = temp_workspace();
    let right_workspace = temp_workspace();
    write_tiny_package_with_source(&left_workspace, "export const value = 1;");
    write_tiny_package_with_source(
        &right_workspace,
        "export const value = 'right workspace has different package bytes';",
    );
    let service = ImportLensService::new(None, false);

    let left = service.handle_batch(batch(&left_workspace, 1));
    let right = service.handle_batch(batch(&right_workspace, 2));

    fs::remove_dir_all(left_workspace).expect("left workspace should be removed");
    fs::remove_dir_all(right_workspace).expect("right workspace should be removed");
    assert!(!left.imports[0].cache_hit);
    assert!(!right.imports[0].cache_hit);
    assert_ne!(left.imports[0].raw_bytes, right.imports[0].raw_bytes);
}

#[test]
fn service_does_not_reuse_cache_across_runtime_profiles() {
    let workspace = temp_workspace();
    write_runtime_package(&workspace);
    let service = ImportLensService::new(None, false);

    let component = service.handle_batch(runtime_batch(&workspace, 1, ImportRuntime::Component));
    let server = service.handle_batch(runtime_batch(&workspace, 2, ImportRuntime::Server));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!component.imports[0].cache_hit);
    assert!(!server.imports[0].cache_hit);
    assert_ne!(component.imports[0].raw_bytes, server.imports[0].raw_bytes);
}

#[test]
fn service_cache_invalidation_removes_matching_package_entries() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let _ = service.handle_batch(batch(&workspace, 1));
    service.invalidate_package("tiny-lib");
    let after_invalidate = service.handle_batch(batch(&workspace, 2));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(!after_invalidate.imports[0].cache_hit);
}

#[test]
fn service_cache_miss_preserves_existing_module_graph_cache() {
    let workspace = temp_workspace();
    let package_root = workspace.join("node_modules").join("graph-cache-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
    let entry_path = workspace
        .join("node_modules")
        .join("graph-cache-lib")
        .join("index.js");
    clear_module_graph_cache();
    let cached_before = build_module_graph_cached(&entry_path).expect("graph should build");
    let service = ImportLensService::new(None, false);

    let response = service.handle_batch(package_batch(&workspace, 3, "graph-cache-lib", "value"));
    let cached_after = build_module_graph_cached(&entry_path).expect("graph should stay cached");

    clear_module_graph_cache();
    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.imports[0].error, None);
    assert!(
        Arc::ptr_eq(&cached_before, &cached_after),
        "service cache misses should reuse valid module graph cache entries",
    );
}

#[test]
fn service_revalidates_cache_when_relative_dependency_changes() {
    let workspace = temp_workspace();
    write_dependent_package(&workspace, "export const helper = 1;");
    let service = ImportLensService::new(None, false);

    let first = service.handle_batch(package_batch(&workspace, 1, "dependent-lib", "value"));
    fs::write(
        workspace
            .join("node_modules")
            .join("dependent-lib")
            .join("helper.js"),
        "export const helper = 'changed dependency payload';",
    )
    .expect("helper should be updated");
    let second = service.handle_batch(package_batch(&workspace, 2, "dependent-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert!(!second.imports[0].cache_hit);
    assert_ne!(first.imports[0].raw_bytes, second.imports[0].raw_bytes);
}

#[test]
fn service_revalidates_cache_when_transitive_package_dependency_changes() {
    let workspace = temp_workspace();
    write_parent_and_transitive_package(&workspace, "export const dep = 1;");
    let service = ImportLensService::new(None, false);

    let first = service.handle_batch(package_batch(&workspace, 1, "parent-lib", "value"));
    fs::write(
        workspace
            .join("node_modules")
            .join("dep-lib")
            .join("index.js"),
        "export const dep = 'changed transitive dependency payload';",
    )
    .expect("dependency should be updated");
    let second = service.handle_batch(package_batch(&workspace, 2, "parent-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert!(!second.imports[0].cache_hit);
    assert_ne!(first.imports[0].raw_bytes, second.imports[0].raw_bytes);
}

#[test]
fn service_caches_full_package_variant_for_conservative_named_imports() {
    let workspace = temp_workspace();
    write_effectful_package(&workspace);
    let service = ImportLensService::new(None, false);

    let named = service.handle_batch(effectful_batch(&workspace, 1, ImportKind::Named));
    let namespace = service.handle_batch(effectful_batch(&workspace, 2, ImportKind::Namespace));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(!named.imports[0].cache_hit);
    assert!(namespace.imports[0].cache_hit);
    assert_eq!(named.imports[0].raw_bytes, namespace.imports[0].raw_bytes);
}

#[test]
fn service_does_not_alias_missing_export_result_to_namespace_cache() {
    let workspace = temp_workspace();
    write_missing_export_effectful_package(&workspace);
    let service = ImportLensService::new(None, false);

    let named = service.handle_batch(missing_effectful_batch(&workspace, 1, ImportKind::Named));
    let namespace = service.handle_batch(missing_effectful_batch(
        &workspace,
        2,
        ImportKind::Namespace,
    ));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!named.imports[0].cache_hit);
    assert!(
        named.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "exports"),
        "{named:?}",
    );
    assert!(!namespace.imports[0].cache_hit);
}

#[test]
fn service_streams_indexed_partials_before_final_response() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);
    let mut request = batch(&workspace, 9);
    request.version = 2;
    request.streaming = true;

    let partials = Mutex::new(Vec::new());
    let final_response = service.handle_batch_streaming(request, |partial| {
        partials
            .lock()
            .expect("partials lock should not be poisoned")
            .push(partial);
    });
    let responses = partials
        .into_inner()
        .expect("partials lock should not be poisoned");

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].indexes, Some(vec![0]));
    assert_eq!(responses[0].imports.len(), 1);
    assert_eq!(final_response.indexes, None);
    assert_eq!(final_response.imports.len(), 1);
}

#[test]
fn service_enumerates_entry_exports_for_completion() {
    let workspace = temp_workspace();
    write_export_package(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.enumerate_exports(enumerate_exports_request(&workspace, 11));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 11);
    assert_eq!(response.error, None);
    assert_eq!(response.exports, vec!["beta", "local", "renamed"]);
}

#[test]
fn service_marks_shared_transitive_modules_in_batch_results() {
    let workspace = temp_workspace();
    write_shared_packages(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_batch(shared_batch(&workspace, 21));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(response.imports.len(), 2);
    assert!(
        response
            .imports
            .iter()
            .all(|result| result.shared_bytes.is_some_and(|bytes| bytes > 0)),
        "{response:?}",
    );
}

#[test]
fn service_marks_shared_bytes_outside_public_top_ten_breakdown() {
    let workspace = temp_workspace();
    write_shared_packages_with_many_unique_modules(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_batch(wide_shared_batch(&workspace, 24));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.imports.len(), 2);
    for result in &response.imports {
        assert_eq!(
            result.module_breakdown.as_ref().map(Vec::len),
            Some(10),
            "{result:?}",
        );
        assert!(
            !result.module_breakdown.as_ref().is_some_and(|modules| {
                modules
                    .iter()
                    .any(|module| module.path.contains("shared-small-util"))
            }),
            "{result:?}",
        );
        assert!(
            result.shared_bytes.is_some_and(|bytes| bytes > 0),
            "{result:?}",
        );
    }
}

#[test]
fn service_computes_file_size_with_shared_module_deduplication() {
    let workspace = temp_workspace();
    write_shared_packages(&workspace);
    let service = ImportLensService::new(None, false);

    let batch = service.handle_batch(shared_batch(&workspace, 22));
    let file_size = service.handle_file_size(file_size_request(&workspace, 23));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    let summed_raw = batch
        .imports
        .iter()
        .map(|result| result.raw_bytes)
        .sum::<u64>();
    assert_eq!(file_size.request_id, 23);
    assert_eq!(file_size.error, None);
    assert!(file_size.raw_bytes > 0);
    assert!(
        file_size.raw_bytes < summed_raw,
        "{file_size:?} >= {summed_raw}"
    );
    assert_eq!(file_size.imports.len(), 2);
    assert!(
        file_size
            .imports
            .iter()
            .all(|result| result.shared_bytes.is_some_and(|bytes| bytes > 0)),
        "{file_size:?}",
    );
}

#[test]
fn service_computes_file_size_for_commonjs_only_imports_conservatively() {
    let workspace = temp_workspace();
    write_cjs_file_size_package(&workspace);
    let service = ImportLensService::new(None, false);

    let file_size = service.handle_file_size(cjs_file_size_request(&workspace, 25));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(file_size.request_id, 25);
    assert_eq!(file_size.error, None);
    assert!(file_size.raw_bytes > 0, "{file_size:?}");
    assert!(file_size.minified_bytes > 0, "{file_size:?}");
    assert_eq!(file_size.imports.len(), 1);
    assert!(
        file_size.diagnostics.iter().any(|diagnostic| {
            diagnostic.stage == "file_size" && diagnostic.message.contains("CommonJS")
        }),
        "{file_size:?}",
    );
}

#[test]
fn service_rejects_v1_export_enumeration_requests() {
    let workspace = temp_workspace();
    write_export_package(&workspace);
    let service = ImportLensService::new(None, false);
    let mut request = enumerate_exports_request(&workspace, 12);
    request.version = 1;

    let response = service.enumerate_exports(request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 12);
    assert!(response.error.is_some());
    assert!(response.exports.is_empty());
}

#[test]
fn protocol_error_batch_response_rejects_all_imports_without_analysis() {
    let workspace = temp_workspace();
    let response = protocol_error_batch_response(
        &batch(&workspace, 42),
        "hello message not received".to_owned(),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 42);
    assert_eq!(response.imports.len(), 1);
    assert_eq!(
        response.imports[0].error.as_deref(),
        Some("hello message not received")
    );
    assert_eq!(response.imports[0].diagnostics[0].stage, "protocol");
    assert_eq!(response.imports[0].raw_bytes, 0);
}

#[test]
fn protocol_error_exports_response_returns_request_scoped_error() {
    let workspace = temp_workspace();
    let response = protocol_error_exports_response(
        &enumerate_exports_request(&workspace, 13),
        "hello message not received".to_owned(),
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 13);
    assert_eq!(response.exports, Vec::<String>::new());
    assert_eq!(
        response.error.as_deref(),
        Some("hello message not received")
    );
    assert_eq!(response.diagnostics[0].stage, "protocol");
}
