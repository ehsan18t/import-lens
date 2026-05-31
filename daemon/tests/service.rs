use import_lens_daemon::{
    ipc::protocol::{
        BatchRequest, EnumerateExportsRequest, ImportKind, ImportRequest, PROTOCOL_VERSION,
    },
    service::{ImportLensService, protocol_error_batch_response, protocol_error_exports_response},
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
    let path = std::env::temp_dir().join(format!("import-lens-service-{suffix}"));
    fs::create_dir_all(&path).expect("temp workspace should be created");
    path
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
        }],
        streaming: false,
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
fn service_streams_indexed_partials_before_final_response() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);
    let mut request = batch(&workspace, 9);
    request.version = 2;
    request.streaming = true;

    let responses = service.handle_batch_streaming(request);

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].indexes, Some(vec![0]));
    assert_eq!(responses[0].imports.len(), 1);
    assert_eq!(responses[1].indexes, None);
    assert_eq!(responses[1].imports.len(), 1);
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
