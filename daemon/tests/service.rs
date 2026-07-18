use import_lens_daemon::{
    ipc::protocol::{
        AnalyzeDocumentRequest, AnalyzePackageJsonRequest, AnalyzeSpecifiersRequest, BatchRequest,
        CacheRemoveRequest, CacheRemoveScope, CacheStatusRequest, CompleteImportMembersRequest,
        EnumerateExportsRequest, FileSizeDocumentRequest, FileSizeRequest, ImportAnalysisStatus,
        ImportKind, ImportRequest, ImportRuntime, PROTOCOL_VERSION, RegistryHintMode,
        RegistryHintTarget,
    },
    pipeline::file_size::FileSizeComputation,
    pipeline::file_size_cache::shared_file_size_cache,
    pipeline::resolver::shared_resolvers,
    registry::service::RegistryHintService,
    service::{ImportLensService, protocol_error_batch_response, protocol_error_exports_response},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

mod common;

// Serializes tests that touch process-global freshness state (the engine's
// dependency-path index and the L1 file-size cache).
static SHARED_INDEX_TEST_LOCK: Mutex<()> = Mutex::new(());

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

/// Same cacheable shape as `write_package`, under a caller-chosen name -- used
/// to prove an invalidation call scoped to one package leaves a sibling
/// package's cache entry alone.
fn write_named_package(workspace: &Path, name: &str) {
    let package_root = workspace.join("node_modules").join(name);
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
}

fn write_versionless_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("versionless-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
}

fn active_document_path(workspace: &Path) -> String {
    workspace
        .join("src")
        .join("index.ts")
        .to_string_lossy()
        .to_string()
}

fn package_json_request(
    workspace: &Path,
    request_id: u64,
    source: &str,
    streaming: bool,
) -> AnalyzePackageJsonRequest {
    AnalyzePackageJsonRequest {
        message_type: "analyze_package_json".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace.join("package.json").to_string_lossy().to_string(),
        source: source.to_owned(),
        include_registry_hints: false,
        force_registry_refresh: false,
        refresh_section: None,
        registry_hint_mode: None,
        streaming,
    }
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

#[test]
fn handle_file_size_populates_and_reuses_aggregate_cache() {
    // It asserts on the process-wide L1 cache, and other tests in this binary CLEAR it (a cache
    // remove, a workspace-config invalidation). Serialize against them.
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let workspace = temp_workspace();
    write_shared_packages(&workspace);
    let service = ImportLensService::new(None, false);

    let request = file_size_request(&workspace, 1);
    let path = PathBuf::from(&request.active_document_path);
    let first = service.handle_file_size(request);
    let second = service.handle_file_size(file_size_request(&workspace, 2));

    // Same import set -> identical aggregate numbers on the repeat request.
    assert_eq!(first.minified_bytes, second.minified_bytes);
    assert_eq!(first.gzip_bytes, second.gzip_bytes);

    // The handler populated L1 for this document. Presence is checked
    // signature-independently so a concurrent generation bump in another test
    // cannot make this assertion flaky.
    assert!(shared_file_size_cache().contains_path(&path));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
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
        cursor_offset: None,
    }
}

#[test]
fn service_analyzes_document_source_in_daemon() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_analyze_document(
        AnalyzeDocumentRequest {
            message_type: "analyze_document".to_owned(),
            version: PROTOCOL_VERSION,
            request_id: 31,
            workspace_root: workspace.to_string_lossy().to_string(),
            active_document_path: active_document_path(&workspace),
            source: "import { value } from 'tiny-lib';\nimport type { Type } from 'tiny-lib';"
                .to_owned(),
        },
        &import_lens_daemon::document::IgnoreRuleResolver::default(),
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 31);
    assert_eq!(response.error, None);
    assert_eq!(response.imports.len(), 1);
    assert_eq!(response.imports[0].status, ImportAnalysisStatus::Ready);
    assert_eq!(response.imports[0].detected.package_name, "tiny-lib");
    assert_eq!(
        response.imports[0]
            .request
            .as_ref()
            .map(|request| request.version.as_str()),
        Some("1.0.0"),
    );
    assert!(response.imports[0].result.is_some());
}

#[test]
fn service_analyzes_raw_specifiers_with_daemon_filtering() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_analyze_specifiers(AnalyzeSpecifiersRequest {
        message_type: "analyze_specifiers".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 32,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: active_document_path(&workspace),
        specifiers: vec![
            "node:fs".to_owned(),
            "./local.js".to_owned(),
            "tiny-lib".to_owned(),
            "https://example.test/mod.js".to_owned(),
            "@/app".to_owned(),
        ],
    });

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 32);
    assert_eq!(response.error, None);
    assert_eq!(response.imports.len(), 1);
    assert_eq!(response.imports[0].detected.specifier, "tiny-lib");
    assert_eq!(response.imports[0].status, ImportAnalysisStatus::Ready);
}

#[test]
fn service_computes_file_size_from_document_source() {
    let workspace = temp_workspace();
    write_shared_packages(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_file_size_document(FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 33,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: active_document_path(&workspace),
        source: "import { left } from 'left-lib';\nimport { right } from 'right-lib';".to_owned(),
        force_fresh: false,
        analysis_generation: None,
    });

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 33);
    assert_eq!(response.error, None);
    assert_eq!(response.states.len(), 2);
    assert_eq!(response.imports.len(), 2);
    assert!(response.raw_bytes > 0, "{response:?}");
    assert!(
        response
            .imports
            .iter()
            .all(|result| result.shared_bytes.is_some_and(|bytes| bytes > 0)),
        "{response:?}",
    );
}

#[test]
fn file_size_document_marks_uncounted_asset_bytes_as_a_floor_and_does_not_cache_it() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let workspace = temp_workspace();
    let package_root = workspace.join("node_modules").join("broken-css-file-cost");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.scss"]}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './broken.scss';\nexport const value = 1;\n",
    )
    .expect("entry should be written");
    fs::write(
        package_root.join("broken.scss"),
        "$brand: red;\n@mixin thing { color: $brand }\n.bad { @include thing }\n",
    )
    .expect("broken stylesheet should be written");

    let service = ImportLensService::new(None, false);
    let document_path = workspace.join("src").join("index.ts");
    let mut first_request = file_size_document_request(
        &workspace,
        34,
        "import { value } from 'broken-css-file-cost';\nconsole.log(value);\n",
    );
    first_request.force_fresh = false;
    let response = service.handle_file_size_document(first_request);

    assert!(
        response.error.is_none(),
        "the JavaScript still builds: {response:?}"
    );
    assert!(
        !response.degraded,
        "the combined build itself succeeds: {response:?}"
    );
    assert!(
        response.raw_bytes > 0,
        "the measured floor remains useful: {response:?}"
    );
    assert!(
        response.incomplete,
        "the response must structurally say that the stylesheet bytes are absent: {response:?}"
    );
    assert!(
        response
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"),
        "the floor must retain its asset disclosure: {response:?}"
    );
    assert!(
        !shared_file_size_cache().contains_path(&document_path),
        "a deterministic import fallback may be reusable, but the partial File Cost may not"
    );

    fs::write(
        package_root.join("broken.scss"),
        ".fixed { color: rebeccapurple; padding: 12345px; }\n",
    )
    .expect("stylesheet should be repaired");
    let mut repaired_request = file_size_document_request(
        &workspace,
        35,
        "import { value } from 'broken-css-file-cost';\nconsole.log(value);\n",
    );
    repaired_request.force_fresh = false;
    let repaired = service.handle_file_size_document(repaired_request);

    assert!(
        repaired.error.is_none(),
        "the repaired file must measure: {repaired:?}"
    );
    assert!(
        !repaired.incomplete,
        "the repaired asset closes the floor: {repaired:?}"
    );
    assert!(
        repaired
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.stage != "uncounted_assets"),
        "the repaired response must lose the stale disclosure: {repaired:?}"
    );
    assert!(
        repaired.raw_bytes > response.raw_bytes,
        "the repaired stylesheet's bytes must enter the total: before={response:?}, after={repaired:?}"
    );
    assert!(
        shared_file_size_cache().contains_path(&document_path),
        "a complete repaired File Cost should enter the aggregate cache"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

/// A workspace manifest naming `tiny-lib`, plus whatever else the caller declares.
fn write_workspace_manifest(workspace: &Path, extra_dependencies: &str) {
    fs::write(
        workspace.join("package.json"),
        format!(r#"{{"name":"app","version":"1.0.0","dependencies":{{"tiny-lib":"1.0.0"{extra_dependencies}}}}}"#),
    )
    .expect("workspace manifest should be written");
}

/// A `tsconfig.json` mapping `@app/*` at first-party source, and the source file it points at — the
/// ordinary shape of a real TypeScript project, and the thing that makes `@app/components` an ALIAS
/// rather than a missing package.
fn write_tsconfig_alias(workspace: &Path) {
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be written");
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
    fs::write(
        workspace.join("src").join("components.ts"),
        "export const Button = 1;\n",
    )
    .expect("aliased source should be written");
}

fn file_size_document_request(
    workspace: &Path,
    request_id: u64,
    source: &str,
) -> FileSizeDocumentRequest {
    file_size_document_request_for(workspace, "src/index.ts", request_id, source)
}

/// A file-size request for a caller-chosen document, so the same import can be asked from a `.ts`,
/// a `.vue`, a `.svelte` and an `.astro` file — which is the whole question in
/// `file_size_document_recognizes_a_path_alias_from_every_supported_document_type`.
fn file_size_document_request_for(
    workspace: &Path,
    document_relative_path: &str,
    request_id: u64,
    source: &str,
) -> FileSizeDocumentRequest {
    let mut document = workspace.to_path_buf();
    for segment in document_relative_path.split('/') {
        document.push(segment);
    }

    FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.to_string_lossy().to_string(),
        source: source.to_owned(),
        force_fresh: true,
        analysis_generation: None,
    }
}

/// **FR-024a, bullet 4 — the fix that nothing could detect.**
///
/// An import of a package that is not installed has no `ImportRequest` (a request carries the
/// installed version), so it is not an entry of the file's combined build and contributes no bytes to
/// any total. It used to be `filter_map`ped out of the aggregate's input in
/// `file_size_document_response` before it could say so: the file's total silently omitted a whole
/// dependency, and — carrying `incomplete: false` — was cached for the L1 TTL, persisted to the
/// no-TTL bundle-impact history as this file's permanent baseline, and passed by `importlens check`
/// with exit 0.
///
/// The fix is one line (`map` over `filter_map`), and reverting it left the entire daemon suite
/// green, because every existing test hands `compute_file_size` a `SizedImport` list that was built
/// by hand. This one goes through the handler, which is where the list is BUILT.
#[test]
fn file_size_document_flags_an_uninstalled_import_as_a_floor() {
    let workspace = temp_workspace();
    write_package(&workspace);
    // `ghost-lib` is declared and was never installed: a missing DEPENDENCY, whose bytes belong in
    // this file's total.
    write_workspace_manifest(&workspace, r#","ghost-lib":"^2.0.0""#);
    let service = ImportLensService::new(None, false);

    let response = service.handle_file_size_document(file_size_document_request(
        &workspace,
        331,
        "import { value } from 'tiny-lib';\nimport { gone } from 'ghost-lib';",
    ));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None, "{response:?}");
    assert_eq!(
        response.states.len(),
        2,
        "test setup: BOTH imports are detected; the question is what the aggregate does with the \
         uninstalled one: {response:?}"
    );
    assert!(
        response.incomplete,
        "an import whose package is not installed leaves this total short by its whole weight - it \
         is a FLOOR, and must never be cached, persisted, or judged: {response:?}"
    );
    assert!(
        response.diagnostics.iter().any(|item| item
            .details
            .iter()
            .any(|detail| detail == "specifier: ghost-lib")),
        "the user is owed the specifier whose bytes are missing: {response:?}"
    );
}

/// **FR-024a bullet 4 says "not installed", and it means it: DECLARATION IS NOT THE DISCRIMINATOR.**
///
/// An earlier attempt at the alias fix used "is the package declared in a `package.json`?" to tell an
/// alias from a missing dependency, and had to narrow this bullet to fit — an import of a package
/// neither declared nor installed (a typo, a stale import after a `pnpm remove`) was then read as an
/// alias and flagged nothing, so a total missing that package's entire weight was cached, persisted
/// as the file's baseline, and passed by `importlens check`. That is the exact silent pass ADR-0006
/// exists to abolish, and declaration cannot justify it: `import _ from 'lodash'` omits the same
/// bytes whether or not `package.json` names lodash.
///
/// So the discriminator is POSITIVE evidence of first-party source (it RESOLVES, through tsconfig
/// `paths`, to a file outside `node_modules`) — never the absence of a declaration. A specifier that
/// resolves to nothing is a floor. This test is what fails if anyone reaches for the declaration
/// again.
#[test]
fn file_size_document_flags_an_undeclared_uninstalled_import_as_a_floor() {
    let workspace = temp_workspace();
    write_package(&workspace);
    // NOT declared, NOT installed, and NOT an alias: nothing in the project mentions `ghost-lib`.
    // A tsconfig with a real alias table is present, and it does not map this specifier either.
    write_workspace_manifest(&workspace, "");
    write_tsconfig_alias(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_file_size_document(file_size_document_request(
        &workspace,
        333,
        "import { value } from 'tiny-lib';\nimport { gone } from 'ghost-lib';",
    ));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None, "{response:?}");
    assert!(
        response.incomplete,
        "an undeclared, uninstalled import omits exactly the bytes a declared one does - it is a \
         FLOOR too, and treating it as an alias is a silent pass on a total missing a whole \
         package: {response:?}"
    );
    assert!(
        response
            .diagnostics
            .iter()
            .any(|item| item.stage == "package_resolution"
                && item
                    .details
                    .iter()
                    .any(|detail| detail == "specifier: ghost-lib")),
        "and it is reported as what it is - an unresolvable package, not an alias: {response:?}"
    );
}

/// **The regression the fix above caused, and the reason it needs a discriminator.**
///
/// A tsconfig PATH ALIAS (`@app/components`) also has no installed package and therefore no request —
/// and it is not a missing dependency at all. It points at first-party source, which Import Lens does
/// not measure (ADR-0004), exactly like a relative import. Reading it as a missing dependency made
/// **every file that uses path aliases a permanent floor**: the combined build re-ran on every size
/// request, nothing was ever cached or persisted, and `importlens check` refused to judge the file.
/// Path aliases are ordinary in real TypeScript projects.
///
/// (`@/…` and `~/…` never reach here — `document::specifier` drops them before detection. It is the
/// alias forms that look like package names, `@app/…`, that were misread.)
#[test]
fn file_size_document_does_not_flag_a_path_alias_as_a_missing_package() {
    let workspace = temp_workspace();
    write_package(&workspace);
    // `@app/*` is an alias in `tsconfig.json` resolving to `src/*`, and it is NOT declared as a
    // dependency. It RESOLVES to first-party source, and that is the whole difference from the two
    // tests above.
    write_workspace_manifest(&workspace, "");
    write_tsconfig_alias(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_file_size_document(file_size_document_request(
        &workspace,
        332,
        "import { value } from 'tiny-lib';\nimport { Button } from '@app/components';",
    ));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None, "{response:?}");
    assert!(
        !response.incomplete,
        "a path alias is first-party source, not an unmeasured dependency: flagging it makes every \
         aliased file a permanent floor - never cached, never persisted, never judged: {response:?}"
    );
    assert!(!response.degraded, "{response:?}");
    assert!(
        response.raw_bytes > 0,
        "the installed package is still measured: {response:?}"
    );
    assert!(
        response
            .diagnostics
            .iter()
            .any(|item| item.stage == "path_alias"),
        "the user is still told why that specifier contributes nothing: {response:?}"
    );
}

/// The same import (`@app/components`), the same project, the same `tsconfig.json` — asked from each
/// of the four document types the extension activates on. **The answer must not depend on which one
/// asks.**
///
/// It did, and that is why the alias fix was dead for half the languages Import Lens supports. The
/// resolution ran through `resolve_file`, which drives `TsconfigDiscovery::Auto`: oxc walks up to the
/// nearest tsconfig that **claims the document** through `files` / `include` / `exclude`, and
/// TypeScript's default `include` claims no `.vue`, `.svelte` or `.astro` file. So the alias resolved
/// from a `.ts` document and resolved to *nothing* from the other three — which the aggregate reads
/// as a package that is not installed. For every Vue, Svelte and Astro user, **every file using a
/// path alias stayed a permanent floor**: never cached, never persisted, and refused a verdict by
/// `importlens check`. Only the `.ts` case was ever tested, which is exactly how it survived.
///
/// "Is this specifier first-party?" is a question about the WORKSPACE'S ALIAS TABLE, not about the
/// document that happens to contain the import. The config is now handed to oxc explicitly, so its
/// `paths` apply whatever the document's extension.
#[test]
fn file_size_document_recognizes_a_path_alias_from_every_supported_document_type() {
    // One tsconfig, no `include` — the ordinary shape, and the one whose TypeScript default claims
    // `.ts` and nothing else. It is what made three of these four documents floors.
    let workspace = temp_workspace();
    write_package(&workspace);
    write_workspace_manifest(&workspace, "");
    write_tsconfig_alias(&workspace);
    let service = ImportLensService::new(None, false);

    let documents = [
        (
            "src/app.ts",
            "import { value } from 'tiny-lib';\nimport { Button } from '@app/components';\n"
                .to_owned(),
        ),
        (
            "src/app.vue",
            "<script setup lang=\"ts\">\nimport { value } from 'tiny-lib';\nimport { Button } from \
             '@app/components';\n</script>\n<template><div /></template>\n"
                .to_owned(),
        ),
        (
            "src/app.svelte",
            "<script lang=\"ts\">\nimport { value } from 'tiny-lib';\nimport { Button } from \
             '@app/components';\n</script>\n<div></div>\n"
                .to_owned(),
        ),
        (
            "src/app.astro",
            "---\nimport { value } from 'tiny-lib';\nimport { Button } from \
             '@app/components';\n---\n<div></div>\n"
                .to_owned(),
        ),
    ];

    let mut failures = Vec::new();
    for (index, (document, source)) in documents.iter().enumerate() {
        let response = service.handle_file_size_document(file_size_document_request_for(
            &workspace,
            document,
            400 + index as u64,
            source,
        ));

        let alias_stage = response
            .diagnostics
            .iter()
            .find(|item| {
                item.details
                    .iter()
                    .any(|detail| detail == "specifier: @app/components")
            })
            .map(|item| item.stage.clone())
            .unwrap_or_else(|| "<no diagnostic>".to_owned());

        failures.push(format!(
            "{document}: stage={alias_stage} incomplete={} states={}",
            response.incomplete,
            response.states.len()
        ));

        assert_eq!(
            response.states.len(),
            2,
            "test setup: both imports must be detected in {document}: {response:?}"
        );
        assert!(
            !response.incomplete,
            "{document}: a path alias resolves to first-party source from EVERY document type. \
             Flagging it here makes every aliased Vue/Svelte/Astro file a permanent floor - never \
             cached, never persisted, never judged. Measured: {failures:?}"
        );
        assert_eq!(
            alias_stage, "path_alias",
            "{document}: the alias must be reported as an alias, not as an unresolvable package. \
             Measured: {failures:?}"
        );
        assert!(
            !response.degraded,
            "{document}: the combined build must still succeed: {response:?}"
        );
        assert!(
            response.raw_bytes > 0,
            "{document}: the installed package is still measured: {response:?}"
        );
    }

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

// ------------------------------------------------------------------------------------------------
// The alias matrix: CONFIG SHAPE x IMPORTING DOCUMENT.
//
// "Does this specifier map, through some `paths` table the workspace reaches, to a first-party file
// that EXISTS?" is one question with one answer, and neither half of this matrix may change it. The
// previous fix proved that both halves can silently break: keying the resolution on the document
// killed `.vue` / `.svelte` / `.astro`, and the repair for THAT (`TsconfigDiscovery::Manual`) killed
// the solution-style config — the literal create-vue and create-astro scaffold — from every document
// type, `.ts` included.
//
// A row per config shape is what makes this a PROPERTY test rather than four examples: add a way of
// spelling an alias table and the matrix demands it work from all four languages, or go red.
// ------------------------------------------------------------------------------------------------

/// One way real projects spell their alias table. Every shape below maps `@app/*` at a real
/// `components.ts` outside `node_modules`, so every shape must answer identically.
///
/// `write` takes the FIXTURE directory and returns the **workspace root** — the directory the client
/// opened. They are the same for every shape but the monorepo one, where the workspace root is one
/// package (`packages/web`) and the alias target sits *above* it.
struct AliasConfigShape {
    name: &'static str,
    write: fn(&Path) -> PathBuf,
}

/// The first-party file every alias in this matrix points at. It EXISTING is the positive evidence
/// that makes the specifier an alias rather than a floor.
fn write_alias_target(workspace: &Path) {
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
    fs::write(
        workspace.join("src").join("components.ts"),
        "export const Button = 1;\n",
    )
    .expect("aliased source should be written");
}

/// The ordinary shape: one `tsconfig.json`, no `include`. TypeScript's default `include` claims
/// `.ts` and nothing else, which is what made the other three documents floors once.
fn write_flat_tsconfig(workspace: &Path) -> PathBuf {
    write_alias_target(workspace);
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be written");
    workspace.to_path_buf()
}

/// The Astro scaffold: an explicit `"include": ["**/*"]`, which claims every document type.
fn write_flat_tsconfig_including_everything(workspace: &Path) -> PathBuf {
    write_alias_target(workspace);
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"include":["**/*"],"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be written");
    workspace.to_path_buf()
}

/// **The monorepo opened at ONE package** — an extremely ordinary way to open one. The workspace root
/// is `packages/web`; its `paths` reach a sibling package **above** the root, and the target that
/// makes the specifier first-party (`packages/shared/components.ts`) is therefore outside the opened
/// workspace entirely.
///
/// It is still the user's own source: it exists, it is not inside `node_modules`, and it ships no
/// npm-package bytes, so it must flag nothing. A previous revision required the target to sit inside
/// the workspace root and made every file using a cross-package alias a **permanent floor**.
fn write_monorepo_package_tsconfig(fixture: &Path) -> PathBuf {
    let shared = fixture.join("packages").join("shared");
    fs::create_dir_all(&shared).expect("shared package should be created");
    fs::write(shared.join("components.ts"), "export const Button = 1;\n")
        .expect("sibling package source should be written");

    let workspace = fixture.join("packages").join("web");
    fs::create_dir_all(workspace.join("src")).expect("web src should be created");
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["../shared/*"]}}}"#,
    )
    .expect("package tsconfig should be written");
    workspace
}

/// The literal create-vue scaffold: a root `tsconfig.json` that is nothing but `references`, with
/// the real `paths` in a referenced `tsconfig.app.json` and a sibling `tsconfig.node.json` that has
/// none. Both referenced projects sit at the workspace root, so a resolver that picks the project
/// whose base directory *contains* the document has nothing to choose on and takes the first — which
/// is why the reference ORDER is a row of its own below.
fn write_solution_style_tsconfig(workspace: &Path, references: &[&str]) -> PathBuf {
    write_alias_target(workspace);
    let references = references
        .iter()
        .map(|path| format!(r#"{{"path":"{path}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    fs::write(
        workspace.join("tsconfig.json"),
        format!(r#"{{"files":[],"references":[{references}]}}"#),
    )
    .expect("root tsconfig should be written");
    fs::write(
        workspace.join("tsconfig.app.json"),
        r#"{"include":["env.d.ts","src/**/*","src/**/*.vue"],"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("app tsconfig should be written");
    fs::write(
        workspace.join("tsconfig.node.json"),
        r#"{"include":["vite.config.*"],"compilerOptions":{"composite":true}}"#,
    )
    .expect("node tsconfig should be written");
    workspace.to_path_buf()
}

fn write_solution_style_paths_project_first(workspace: &Path) -> PathBuf {
    write_solution_style_tsconfig(workspace, &["./tsconfig.app.json", "./tsconfig.node.json"])
}

/// The order create-vue actually ships: the project WITHOUT `paths` is listed first.
fn write_solution_style_paths_project_last(workspace: &Path) -> PathBuf {
    write_solution_style_tsconfig(workspace, &["./tsconfig.node.json", "./tsconfig.app.json"])
}

/// **A solution-style config with a STALE reference.** `tsconfig.node.json` was deleted and nobody
/// updated the `references` list — not exotic, and it used to cost the workspace *every* alias table:
/// the enumeration asked oxc to resolve the root config with its references, which fails whole if any
/// one of them cannot be read, so the `tsconfig.app.json` that owns the only `paths` table was never
/// asked. A bad reference must cost that project's table and no other.
fn write_solution_style_with_a_dangling_reference(workspace: &Path) -> PathBuf {
    write_solution_style_tsconfig(workspace, &["./tsconfig.node.json", "./tsconfig.app.json"]);
    fs::remove_file(workspace.join("tsconfig.node.json")).expect("node tsconfig should be removed");
    workspace.to_path_buf()
}

/// A JavaScript project declares its aliases in `jsconfig.json` and in nothing else.
fn write_jsconfig(workspace: &Path) -> PathBuf {
    write_alias_target(workspace);
    fs::write(
        workspace.join("jsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("jsconfig should be written");
    workspace.to_path_buf()
}

/// The `paths` live in a base config the project `extends`.
fn write_extending_tsconfig(workspace: &Path) -> PathBuf {
    write_alias_target(workspace);
    fs::write(
        workspace.join("tsconfig.base.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("base tsconfig should be written");
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"extends":"./tsconfig.base.json"}"#,
    )
    .expect("tsconfig should be written");
    workspace.to_path_buf()
}

const ALIAS_CONFIG_SHAPES: [AliasConfigShape; 8] = [
    AliasConfigShape {
        name: "flat tsconfig, default include",
        write: write_flat_tsconfig,
    },
    AliasConfigShape {
        name: "flat tsconfig, include **/*",
        write: write_flat_tsconfig_including_everything,
    },
    AliasConfigShape {
        name: "solution-style, paths project first",
        write: write_solution_style_paths_project_first,
    },
    AliasConfigShape {
        name: "solution-style, paths project last",
        write: write_solution_style_paths_project_last,
    },
    AliasConfigShape {
        name: "solution-style, dangling reference",
        write: write_solution_style_with_a_dangling_reference,
    },
    AliasConfigShape {
        name: "jsconfig",
        write: write_jsconfig,
    },
    AliasConfigShape {
        name: "extends a base config",
        write: write_extending_tsconfig,
    },
    AliasConfigShape {
        name: "monorepo, target above the root",
        write: write_monorepo_package_tsconfig,
    },
];

/// The same import, asked from each of the four document types the extension activates on: a `.ts`,
/// a `.vue`, a `.svelte` and an `.astro` file. Every one of them also imports the installed
/// `tiny-lib`, so the file always has a real combined build to be a total OF.
fn documents_importing(specifier: &str) -> [(&'static str, String); 4] {
    [
        (
            "src/app.ts",
            format!(
                "import {{ value }} from 'tiny-lib';\nimport {{ Button }} from '{specifier}';\n"
            ),
        ),
        (
            "src/app.vue",
            format!(
                "<script setup lang=\"ts\">\nimport {{ value }} from 'tiny-lib';\nimport {{ Button \
                 }} from '{specifier}';\n</script>\n<template><div /></template>\n"
            ),
        ),
        (
            "src/app.svelte",
            format!(
                "<script lang=\"ts\">\nimport {{ value }} from 'tiny-lib';\nimport {{ Button }} \
                 from '{specifier}';\n</script>\n<div></div>\n"
            ),
        ),
        (
            "src/app.astro",
            format!(
                "---\nimport {{ value }} from 'tiny-lib';\nimport {{ Button }} from \
                 '{specifier}';\n---\n<div></div>\n"
            ),
        ),
    ]
}

/// The stage the aggregate reported for `specifier` — `path_alias` (first-party source, flags
/// nothing) or `package_resolution` (a floor). The one fact this whole matrix is about.
fn stage_for_specifier(
    response: &import_lens_daemon::ipc::protocol::FileSizeDocumentResponse,
    specifier: &str,
) -> String {
    response
        .diagnostics
        .iter()
        .find(|item| {
            item.details
                .iter()
                .any(|detail| detail == &format!("specifier: {specifier}"))
        })
        .map(|item| item.stage.clone())
        .unwrap_or_else(|| "<no diagnostic>".to_owned())
}

/// **The matrix. Eight config shapes x four document types, and the answer must be the same in all
/// thirty-two cells.**
///
/// An earlier fix handed the nearest config to oxc with `TsconfigDiscovery::Manual` — which is right,
/// and is what makes the `paths` apply regardless of what `include` claims — but left oxc to CHOOSE a
/// project out of `references` (`TsconfigReferences::Auto`). With `include` no longer distinguishing
/// them, that choice falls back to "the first referenced project whose base directory contains the
/// document", and in a solution-style config every referenced project sits at the workspace root, so
/// all of them contain it and the tie breaks on **`references` list order**. The create-vue scaffold
/// lists `tsconfig.node.json` first; it has no `paths`; the alias resolved to nothing from every
/// document type, `.ts` included. The SRS, the commit message and two doc comments all claimed
/// solution-style configs worked, and **no test covered a `references` config at all**.
///
/// The question is document-independent and project-independent: *does this specifier map, through
/// ANY `paths` table the workspace reaches, to a first-party file that exists?* So the daemon asks
/// every reachable table and stops asking oxc to pick one.
///
/// Two rows are here because the answer must not depend on the workspace being *whole*, either:
/// a **stale `references` entry** (the deleted project every scaffold eventually accumulates) must
/// cost only its own table, and a **monorepo opened at one package** must still see an alias target
/// that sits above the root — it is the user's own source, and it weighs nothing.
#[test]
fn a_path_alias_resolves_from_every_config_shape_and_every_document_type() {
    let mut measured = Vec::new();
    let mut failures = Vec::new();

    for (shape_index, shape) in ALIAS_CONFIG_SHAPES.iter().enumerate() {
        let fixture = temp_workspace();
        // The workspace root is the directory the client opened, and it is NOT always the fixture
        // root: the monorepo shape opens one package of it.
        let workspace = (shape.write)(&fixture);
        write_package(&workspace);
        write_workspace_manifest(&workspace, "");
        let service = ImportLensService::new(None, false);

        for (document_index, (document, source)) in
            documents_importing("@app/components").iter().enumerate()
        {
            let response = service.handle_file_size_document(file_size_document_request_for(
                &workspace,
                document,
                500 + (shape_index * 10 + document_index) as u64,
                source,
            ));

            let stage = stage_for_specifier(&response, "@app/components");
            let cell = format!(
                "{:<36} x {:<14} stage={:<20} incomplete={:<5} degraded={:<5} states={}",
                shape.name,
                document,
                stage,
                response.incomplete,
                response.degraded,
                response.states.len()
            );
            let ok = !response.incomplete
                && !response.degraded
                && stage == "path_alias"
                && response.states.len() == 2
                && response.raw_bytes > 0;
            if !ok {
                failures.push(cell.clone());
            }
            measured.push(cell);
        }

        fs::remove_dir_all(&fixture).expect("temp workspace should be removed");
    }

    println!("alias matrix:\n{}", measured.join("\n"));
    assert!(
        failures.is_empty(),
        "an alias must resolve from EVERY config shape and EVERY document type - a cell that does \
         not is a file permanently refused a cache entry, a persisted baseline and a verdict.\n\
         FAILED:\n{}\n\nFULL MATRIX:\n{}",
        failures.join("\n"),
        measured.join("\n")
    );
}

/// One specifier that must stay a **floor**, and the workspace shape that makes it one.
struct FloorCase {
    name: &'static str,
    write: fn(&Path),
    specifier: &'static str,
    /// Whether the workspace's `package.json` declares the specifier. Declaration is **not** the
    /// discriminator and must change nothing: `import _ from 'lodash'` omits the same bytes whether
    /// or not the manifest names lodash.
    declared: bool,
}

/// An alias table that is real and simply does not map this specifier.
fn write_flat_tsconfig_for_floor(workspace: &Path) {
    let _workspace_root = write_flat_tsconfig(workspace);
}

/// No `tsconfig.json`, no `jsconfig.json`: nothing the daemon can read, so there is no positive
/// evidence to be had and a bare specifier that is not installed is a floor.
fn write_no_config(workspace: &Path) {
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
}

/// The alias PATTERN matches (`@app/*`), and the file it points at does not exist. Positive evidence
/// is the file, not the pattern.
fn write_flat_tsconfig_without_the_target(workspace: &Path) {
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be written");
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
}

const FLOOR_CASES: [FloorCase; 5] = [
    FloorCase {
        name: "not installed, declared",
        write: write_flat_tsconfig_for_floor,
        specifier: "ghost-lib",
        declared: true,
    },
    FloorCase {
        name: "not installed, undeclared",
        write: write_flat_tsconfig_for_floor,
        specifier: "phantom-lib",
        declared: false,
    },
    FloorCase {
        name: "typo of an installed package",
        write: write_flat_tsconfig_for_floor,
        specifier: "tiny-lob",
        declared: false,
    },
    FloorCase {
        name: "alias whose target does not exist",
        write: write_flat_tsconfig_without_the_target,
        specifier: "@app/missing",
        declared: false,
    },
    FloorCase {
        name: "no config at all",
        write: write_no_config,
        specifier: "ghost-lib",
        declared: false,
    },
];

/// **The other half of the matrix, and the half that must never soften.** Widening what counts as an
/// alias is exactly how the silent pass ADR-0006 abolishes gets reintroduced: a total short a whole
/// package, cached, persisted as the file's baseline, and passed by `importlens check` with exit 0.
///
/// So every one of these — a package that is simply not installed (declared or not), a typo, an alias
/// whose target file does not exist, a bare specifier in a project with no config at all — must stay
/// a FLOOR from every document type. The discriminator is positive evidence of first-party SOURCE:
/// a file that **exists** and is **not under `node_modules`**. That is the whole test — the target
/// does *not* have to sit inside the workspace root, because a monorepo alias (`"@shared/*":
/// ["../shared/*"]`) points at a sibling package above the opened root and that is still the user's
/// own code — which is what the alias matrix's "monorepo, target above the root" row is for. A
/// pattern that matches is not evidence, and a missing declaration is not evidence of anything at
/// all.
#[test]
fn an_unresolvable_specifier_stays_a_floor_from_every_document_type() {
    let mut measured = Vec::new();
    let mut failures = Vec::new();

    for (case_index, case) in FLOOR_CASES.iter().enumerate() {
        let workspace = temp_workspace();
        write_package(&workspace);
        write_workspace_manifest(
            &workspace,
            if case.declared {
                r#","ghost-lib":"^2.0.0""#
            } else {
                ""
            },
        );
        (case.write)(&workspace);
        let service = ImportLensService::new(None, false);

        for (document_index, (document, source)) in
            documents_importing(case.specifier).iter().enumerate()
        {
            let response = service.handle_file_size_document(file_size_document_request_for(
                &workspace,
                document,
                600 + (case_index * 10 + document_index) as u64,
                source,
            ));

            let stage = stage_for_specifier(&response, case.specifier);
            let cell = format!(
                "{:<36} x {:<14} specifier={:<16} stage={:<20} incomplete={}",
                case.name, document, case.specifier, stage, response.incomplete
            );
            let ok =
                response.incomplete && stage == "package_resolution" && response.states.len() == 2;
            if !ok {
                failures.push(cell.clone());
            }
            measured.push(cell);
        }

        fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    }

    println!("floor matrix:\n{}", measured.join("\n"));
    assert!(
        failures.is_empty(),
        "a specifier that resolves to no first-party file is a FLOOR from every document type. \
         Reading one as an alias caches a total that is missing a whole package, persists it as the \
         file's baseline, and passes it in CI - the silent pass ADR-0006 exists to abolish.\n\
         FAILED:\n{}\n\nFULL MATRIX:\n{}",
        failures.join("\n"),
        measured.join("\n")
    );
}

/// **The remedy the SRS prescribes must actually work — and the memo that can still hide it.**
///
/// A file whose alias the daemon cannot resolve is a floor, and the SRS tells the developer to repair
/// it by adding the `paths` entry to `tsconfig.json`. They did — and nothing happened, for the rest of
/// the daemon's life, because the alias resolvers were memoized and `oxc_resolver` memoizes the parsed
/// config in a resolver's FS cache. Two things fix that, and they are not the same thing:
///
/// **Part 1 — the config itself is re-read per query.** The alias resolvers are built fresh for each
/// question and memoize no filesystem fact (that is what stops a floor being sticky, FR-024a), so a
/// `paths` edit lands on the next request with **no message at all**. This half goes red the moment
/// anyone memoizes a resolver again.
///
/// **Part 2 — the `references` GRAPH is memoized, and only the watcher can drop it.** Which configs
/// the workspace reaches is walked once and cached per nearest-config path, so a config that starts
/// **referencing** the project that owns the `paths` is invisible until `invalidate_shared_resolvers`
/// runs. That is what the tsconfig watcher (FR-027a) still buys, and this half goes red without it.
#[test]
fn a_workspace_config_change_invalidates_the_memoized_alias_table() {
    // Clearing the shared L1 aggregate cache is part of this invalidation, so serialize against the
    // other tests that read it.
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let workspace = temp_workspace();
    write_package(&workspace);
    write_workspace_manifest(&workspace, "");
    // A real tsconfig, and real first-party targets — but NO `@app/*` entry yet. This is the state
    // the developer is in when the daemon tells them their file is a floor.
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
    fs::write(
        workspace.join("src").join("components.ts"),
        "export const Button = 1;\n",
    )
    .expect("aliased source should be written");
    fs::write(
        workspace.join("src").join("widget.ts"),
        "export const Widget = 1;\n",
    )
    .expect("aliased source should be written");
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":"."}}"#,
    )
    .expect("tsconfig should be written");

    let service = ImportLensService::new(None, false);
    let source = "import { value } from 'tiny-lib';\nimport { Button } from '@app/components';";

    // 1. Without the alias entry the specifier resolves to nothing, so the total is a floor. This is
    //    correct, and it is also what USED to memoize the config in the resolver's cache.
    let before =
        service.handle_file_size_document(file_size_document_request(&workspace, 410, source));
    assert!(
        before.incomplete,
        "test setup: with no `@app/*` entry the specifier resolves to nothing and the total is a \
         floor: {before:?}"
    );

    // 2. The developer applies the exact repair the SRS prescribes — and it takes effect on the very
    //    next request, with no watcher event at all, because nothing memoized the config.
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be rewritten");

    let repaired =
        service.handle_file_size_document(file_size_document_request(&workspace, 411, source));
    assert!(
        !repaired.incomplete,
        "the alias table changed on disk and the daemon must see it. It did not: the parsed config \
         was memoized in the resolver's FS cache forever, so the repair the SRS prescribes did \
         nothing at all: {repaired:?}"
    );
    assert!(
        repaired
            .diagnostics
            .iter()
            .any(|item| item.stage == "path_alias"),
        "and the specifier is now reported as the alias it became: {repaired:?}"
    );

    // 3. Now a change no re-read can see: the project gains a REFERENCED project, and that project
    //    owns the only table mapping `@lib/*`. Which configs the workspace reaches is memoized, so
    //    this is invisible until the watcher says the config changed.
    fs::write(
        workspace.join("tsconfig.app.json"),
        r#"{"include":["src/**/*"],"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["src/*"]}}}"#,
    )
    .expect("app tsconfig should be written");
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"references":[{"path":"./tsconfig.app.json"}],"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be rewritten");

    let referenced_source =
        "import { value } from 'tiny-lib';\nimport { Widget } from '@lib/widget';";
    let invalidated = service.invalidate_workspace_config_paths(&[workspace
        .join("tsconfig.json")
        .to_string_lossy()
        .to_string()]);
    let after = service.handle_file_size_document(file_size_document_request(
        &workspace,
        412,
        referenced_source,
    ));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(invalidated, "a non-empty config batch must invalidate");
    assert!(
        !after.incomplete,
        "the workspace now REACHES a config it did not reach before, and the reachable-config walk \
         is memoized: without the watcher dropping the shared resolvers, the referenced project's \
         alias table is never asked and the file is a floor forever: {after:?}"
    );
    assert!(
        after
            .diagnostics
            .iter()
            .any(|item| item.stage == "path_alias"),
        "and the referenced project's alias is reported as an alias: {after:?}"
    );
}

/// **THE FLOOR MUST NOT BE STICKY, and no message can lift this one.**
///
/// A developer writes `import { Button } from '@app/components'` before creating
/// `src/components.ts` — the ordinary order in which code gets written. The alias resolves to nothing,
/// so the file is correctly a floor. Then they create the file.
///
/// The floor stayed, for the rest of the daemon's life. The alias resolvers were memoized per config,
/// and `oxc_resolver` negative-caches a missing path in the resolver's filesystem cache: the daemon's
/// first answer for that specifier was its answer forever. **A cached negative that nothing
/// invalidates** — the same defect as the tsconfig the daemon read exactly once, one level down, and
/// worse, because nothing watches first-party source, so there is no message that could lift it.
/// The file was never cached, never persisted, and refused a verdict by `importlens check`.
///
/// So the alias resolvers are built per query and memoize no filesystem fact. **This test creates the
/// target mid-flight and asserts the floor lifts with no invalidation and no restart** — the same
/// `ImportLensService`, the same process.
#[test]
fn creating_the_alias_target_lifts_the_floor_without_any_invalidation() {
    let workspace = temp_workspace();
    write_package(&workspace);
    write_workspace_manifest(&workspace, "");
    // A real alias table, pointing at a file that does not exist yet.
    fs::create_dir_all(workspace.join("src")).expect("src should be created");
    fs::write(
        workspace.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .expect("tsconfig should be written");

    let service = ImportLensService::new(None, false);
    let source = "import { value } from 'tiny-lib';\nimport { Button } from '@app/components';";

    // 1. The import is written before the component. The specifier resolves to nothing, so the file
    //    is a floor — correct, and it is also what used to memoize the miss.
    let before =
        service.handle_file_size_document(file_size_document_request(&workspace, 420, source));
    assert!(
        before.incomplete,
        "test setup: the alias target does not exist yet, so there is no positive evidence and the \
         total is a floor: {before:?}"
    );

    // 2. They create it. No `invalidate_workspace_config_paths`, no `node_modules_changed`, no
    //    restart — nothing watches first-party source, so nothing could tell the daemon anyway.
    fs::write(
        workspace.join("src").join("components.ts"),
        "export const Button = 1;\n",
    )
    .expect("aliased source should be written");

    let after =
        service.handle_file_size_document(file_size_document_request(&workspace, 421, source));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(
        !after.incomplete,
        "creating the alias target must lift the floor. It did not: the miss was cached inside the \
         memoized resolver's filesystem cache, so this file stayed a floor for the daemon's life - \
         never cached, never persisted, and refused a verdict by `importlens check`: {after:?}"
    );
    assert!(
        after
            .diagnostics
            .iter()
            .any(|item| item.stage == "path_alias"),
        "and the specifier is now reported as the alias it became: {after:?}"
    );
    assert!(
        after.raw_bytes > 0,
        "the installed package is still measured: {after:?}"
    );
}

/// The other half of the invalidation contract: an EMPTY batch is a no-op. Without this, "invalidate
/// on anything" would pass the test above while dropping every resolver on every unrelated watcher
/// event.
#[test]
fn an_empty_workspace_config_batch_invalidates_nothing() {
    let service = ImportLensService::new(None, false);
    assert!(!service.invalidate_workspace_config_paths(&[]));
}

#[test]
fn file_size_document_force_fresh_bypasses_serve_stale() {
    use import_lens_daemon::ipc::protocol::FreshnessKind;

    let workspace = temp_workspace();
    write_dependent_package(&workspace, "export const helper = 1;");
    let service = ImportLensService::new(None, false);
    let document = active_document_path(&workspace);
    let source = "import { value } from 'dependent-lib';".to_owned();
    let make = |request_id: u64, force_fresh: bool| FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.clone(),
        source: source.clone(),
        force_fresh,
        analysis_generation: None,
    };

    // Populate the cache (Fresh), then change the transitive dependency + bump the
    // generation so the entry is stale.
    let baseline = service.handle_file_size_document(make(1, false)).raw_bytes;
    fs::write(
        workspace
            .join("node_modules")
            .join("dependent-lib")
            .join("helper.js"),
        "export const helper = 'a substantially larger changed dependency payload value';",
    )
    .expect("helper should be updated");
    import_lens_daemon::cache::memory::bump_cache_generation();

    // force_fresh = false → stale-while-revalidate: serve the last-known value flagged
    // Stale, size unchanged.
    let stale = service.handle_file_size_document(make(2, false));
    assert!(
        stale
            .imports
            .iter()
            .any(|result| matches!(result.freshness.kind, FreshnessKind::Stale)),
        "force_fresh=false serves the stale value flagged Stale: {stale:?}"
    );

    // force_fresh = true → recompute synchronously: Fresh, and reflecting the changed
    // dependency (different size), never the stale value.
    let fresh = service.handle_file_size_document(make(3, true));
    assert!(
        fresh
            .imports
            .iter()
            .all(|result| matches!(result.freshness.kind, FreshnessKind::Fresh)),
        "force_fresh=true recomputes fresh: {fresh:?}"
    );
    assert_ne!(
        fresh.raw_bytes, baseline,
        "force_fresh reflects the changed dependency"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[test]
fn force_fresh_file_cost_tracks_css_children_and_local_assets_immediately() {
    let _shared = SHARED_INDEX_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    shared_file_size_cache().clear();

    let workspace = temp_workspace();
    let package_root = workspace.join("node_modules").join("font-cost-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.css"]}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './styles.css';\nexport const value = 1;\n",
    )
    .expect("entry should be written");
    fs::write(
        package_root.join("styles.css"),
        "@import './child.css';\n.entry { color: blue; }\n",
    )
    .expect("stylesheet should be written");
    fs::write(
        package_root.join("child.css"),
        "@font-face { font-family: Probe; src: url('./probe.woff2'); }\n\
         .child { color: red; font-family: Probe; }\n",
    )
    .expect("child stylesheet should be written");
    fs::write(package_root.join("probe.woff2"), vec![0x21; 1024]).expect("font should be written");

    let service = ImportLensService::new(None, false);
    let source = "import { value } from 'font-cost-lib';";
    let first =
        service.handle_file_size_document(file_size_document_request(&workspace, 501, source));
    let first_font = first.imports[0]
        .asset_breakdown
        .iter()
        .find(|contribution| contribution.kind == import_lens_daemon::engine::AssetKind::Font)
        .expect("the initial font should be counted");
    assert_eq!(first_font.raw_bytes, 1024, "{first:?}");

    // Only the successfully bundled child changes. It is not a Rolldown graph module, so this
    // specifically guards the exact CSS-provider observations used by the File Cost L1 cache.
    fs::write(
        package_root.join("child.css"),
        "@font-face { font-family: Probe; src: url('./probe.woff2'); }\n\
         .child { color: rebeccapurple; font-family: Probe; padding: 12345px; margin: 67890px; }\n",
    )
    .expect("child stylesheet should be updated");
    let after_child =
        service.handle_file_size_document(file_size_document_request(&workspace, 502, source));
    assert!(
        after_child.raw_bytes > first.raw_bytes,
        "File Cost must immediately include a successful child stylesheet edit: first={first:?}, after_child={after_child:?}"
    );

    fs::write(package_root.join("probe.woff2"), vec![0x43; 5 * 1024])
        .expect("font should be updated");
    let after_font =
        service.handle_file_size_document(file_size_document_request(&workspace, 503, source));
    let after_font_contribution = after_font.imports[0]
        .asset_breakdown
        .iter()
        .find(|contribution| contribution.kind == import_lens_daemon::engine::AssetKind::Font)
        .expect("the refreshed font should be counted");

    shared_file_size_cache().clear();
    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(
        after_font_contribution.raw_bytes,
        5 * 1024,
        "force-fresh import analysis must see the edited font: {after_font:?}"
    );
    assert_eq!(
        after_font.raw_bytes.checked_sub(after_child.raw_bytes),
        Some(4 * 1024),
        "File Cost must not serve its old L1 value after the asset changed: after_child={after_child:?}, after_font={after_font:?}"
    );
}

#[test]
fn file_size_document_force_fresh_recomputes_on_unknown_dependency() {
    // §4.5 / Finding 13b: the force-fresh (CI / `importlens check`) path serves cache
    // via the evicting `get`, which on `Freshness::Unknown` (a transient stat/read
    // error on a dependency) KEEPS the entry and returns it with `cache_hit = true`
    // and freshness left at its stored default (`Fresh`) — so CI could judge a budget
    // against an unverified last-known value it is told is verified. force_fresh must
    // recompute synchronously instead of serving that laundered value.
    let workspace = temp_workspace();
    write_dependent_package(&workspace, "export const helper = 1;");
    let service = ImportLensService::new(None, false);
    let document = active_document_path(&workspace);
    let source = "import { value } from 'dependent-lib';".to_owned();
    let make = |request_id: u64, force_fresh: bool| FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.clone(),
        source: source.clone(),
        force_fresh,
        analysis_generation: None,
    };

    // Populate the cache (Fresh).
    service.handle_file_size_document(make(1, false));

    // Sanity check: with the dependency untouched, force_fresh still serves the
    // genuinely verified Fresh hit — the fast path this fix must not regress.
    let still_fresh = service.handle_file_size_document(make(2, true));
    assert_eq!(still_fresh.imports.len(), 1, "{still_fresh:?}");
    assert!(
        still_fresh.imports[0].cache_hit,
        "force_fresh should still serve a genuinely verified Fresh hit: {still_fresh:?}"
    );

    // Make the cached entry's dependency fingerprint unverifiable: swap the real
    // `helper.js` (its cached fingerprint carries a content hash captured at analysis
    // time) for a directory at the same path. The mtime+len pre-filter never matches a
    // directory, so `check_fingerprint` falls to its content-hash `fs::read`, which
    // fails on a directory with a non-`NotFound` error → `Freshness::Unknown`
    // (deterministic, no mocking — the B3 technique from
    // freshness_core.rs/result_freshness.rs). Bump the generation so the lookup takes
    // the slow re-verify path instead of the TTL fast path (mirrors
    // `file_size_document_force_fresh_bypasses_serve_stale` above).
    let helper_path = workspace
        .join("node_modules")
        .join("dependent-lib")
        .join("helper.js");
    fs::remove_file(&helper_path).expect("helper.js should be removable");
    fs::create_dir(&helper_path).expect("helper.js path should become a directory");
    import_lens_daemon::cache::memory::bump_cache_generation();

    // force_fresh = true, dependency now Unknown → must recompute synchronously,
    // never serve the cached value just because the evicting `get` treats Unknown as
    // "keep". `cache_hit` is set exclusively by the cache-serve paths (never by a
    // fresh `analyze_and_cache`), so `cache_hit == false` is a direct signal that a
    // real recompute ran instead of a cache serve.
    let recomputed = service.handle_file_size_document(make(3, true));
    assert_eq!(recomputed.imports.len(), 1, "{recomputed:?}");
    assert!(
        !recomputed.imports[0].cache_hit,
        "force_fresh must recompute (never serve the cached value) when the dependency \
         cannot be verified (Unknown): {recomputed:?}"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[test]
fn entry_with_missing_dependency_self_heals_on_access() {
    use import_lens_daemon::ipc::protocol::FreshnessKind;

    // §7 / §4.3: a cached entry whose transitive dependency is DELETED (NotFound ->
    // Gone) is evicted and recomputed on the next NORMAL access -- proving path-missing
    // reclaim needs no orphan scan. This exercises the freshness probe alone (no
    // name-invalidation call): a GONE dependency, unlike a merely CHANGED one (Stale,
    // which is served stale-while-revalidating -- see
    // `file_size_document_force_fresh_bypasses_serve_stale`), must NEVER be served
    // stale. It evicts even on the serve-stale read path and recomputes synchronously.
    let workspace = temp_workspace();
    write_dependent_package(&workspace, "export const helper = 1;");
    let service = ImportLensService::new(None, false);
    let document = active_document_path(&workspace);
    let source = "import { value } from 'dependent-lib';".to_owned();
    // NORMAL (serve-stale) reads throughout -- force_fresh stays false, so any recompute
    // is driven by the freshness probe evicting the Gone entry, not by a force-fresh
    // bypass.
    let make = |request_id: u64| FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.clone(),
        source: source.clone(),
        force_fresh: false,
        analysis_generation: None,
    };

    // Populate the cache, then confirm the second normal read is genuinely a cache hit.
    let baseline = service.handle_file_size_document(make(1));
    assert_eq!(baseline.imports.len(), 1, "{baseline:?}");
    let cached = service.handle_file_size_document(make(2));
    assert!(
        cached.imports[0].cache_hit,
        "precondition: the entry must be cached before the dependency is deleted: {cached:?}"
    );

    // Delete the transitive dependency (helper.js): its next fingerprint stat reports
    // NotFound -> Gone. Bump the generation as a real node_modules mutation would (the
    // extension fires NodeModulesChanged), taking the entry off the TTL fast path onto
    // re-verification -- the same seam the automatic reclaim rides in production.
    let helper_path = workspace
        .join("node_modules")
        .join("dependent-lib")
        .join("helper.js");
    fs::remove_file(&helper_path).expect("helper.js should be removable");
    import_lens_daemon::cache::memory::bump_cache_generation();

    // Next NORMAL access self-heals: the Gone dependency evicts the entry and a real
    // recompute runs. `cache_hit` is set exclusively by the cache-serve paths, so
    // `cache_hit == false` is a direct signal the entry was reclaimed and recomputed,
    // never served stale.
    let healed = service.handle_file_size_document(make(3));

    fs::remove_dir_all(&workspace).ok();
    assert_eq!(healed.imports.len(), 1, "{healed:?}");
    assert!(
        !healed.imports[0].cache_hit,
        "a Gone dependency must evict + recompute on access, never serve stale: {healed:?}"
    );
    assert!(
        !matches!(healed.imports[0].freshness.kind, FreshnessKind::Stale),
        "path-missing reclaim recomputes fresh; a Gone entry is never flagged Stale: {healed:?}"
    );
}

#[test]
fn revalidate_document_sizes_recomputes_only_stale_specifiers() {
    use std::collections::HashSet;

    let workspace = temp_workspace();
    write_dependent_package(&workspace, "export const helper = 1;");
    write_tiny_package_with_source(&workspace, "export const value = 'tiny';");
    let service = ImportLensService::new(None, false);
    let document = active_document_path(&workspace);
    let source =
        "import { value } from 'dependent-lib';\nimport { value as t } from 'tiny-lib';".to_owned();
    let request = FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document,
        source,
        force_fresh: false,
        analysis_generation: None,
    };

    // Only `dependent-lib` is flagged stale. A fresh sibling (`tiny-lib`) must NOT be
    // recomputed — one changed dep must not trigger a full re-analysis of the file.
    let stale = HashSet::from(["dependent-lib".to_owned()]);
    let (_, _, results, identities) = service
        .revalidate_document_sizes(&request, &stale, || true)
        .expect("a stale specifier should produce a refreshed result");

    let specifiers = results
        .iter()
        .map(|result| result.specifier.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        specifiers,
        vec!["dependent-lib"],
        "only the stale specifier is recomputed, not the fresh sibling: {specifiers:?}"
    );
    // The push carries a per-import identity index-aligned 1:1 with the results.
    assert_eq!(
        identities.len(),
        results.len(),
        "identities are index-aligned with results"
    );
    assert_eq!(identities[0].specifier, "dependent-lib");

    // An empty stale set is a no-op (nothing to revalidate).
    assert!(
        service
            .revalidate_document_sizes(&request, &HashSet::new(), || true)
            .is_none(),
        "an empty stale set recomputes nothing"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[test]
fn revalidate_document_sizes_bails_when_superseded() {
    use std::collections::HashSet;

    // F3-B: a background revalidation whose document has been superseded (its
    // continuation predicate returns false) must bail before the expensive
    // recompute rather than recomputing a result no client will use.
    let workspace = temp_workspace();
    write_dependent_package(&workspace, "export const helper = 1;");
    let service = ImportLensService::new(None, false);
    let request = FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: active_document_path(&workspace),
        source: "import { value } from 'dependent-lib';".to_owned(),
        force_fresh: false,
        analysis_generation: None,
    };
    let stale = HashSet::from(["dependent-lib".to_owned()]);

    assert!(
        service
            .revalidate_document_sizes(&request, &stale, || false)
            .is_none(),
        "a superseded revalidation recomputes nothing"
    );
    // Control: with a live continuation the same stale specifier still recomputes.
    assert!(
        service
            .revalidate_document_sizes(&request, &stale, || true)
            .is_some(),
        "a live revalidation still recomputes the stale specifier"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

/// The SWR push and the cache write are gated by the SAME predicate (`should_cache_result`), and
/// under ADR-0006 that predicate means one thing: **is this outcome durable?** So the push now
/// carries exactly what the cache took.
///
/// A DETERMINISTIC failure is both. The import genuinely cannot be sized — it will fail the same
/// way every time — so the daemon caches it, and the client is owed the fact: withholding the
/// push would leave a stale number on screen for code that no longer produces one. It arrives with
/// **no size**, which is what makes that safe; before this change the same push would have carried
/// a fabricated one. (A TRANSIENT failure is neither cached nor pushed — the property test in
/// `service.rs` quantifies that over every transient stage.)
#[test]
fn revalidate_document_sizes_pushes_a_deterministic_failure_carrying_no_size() {
    use std::collections::HashSet;

    let workspace = temp_workspace();
    write_missing_export_effectful_package(&workspace);
    let service = ImportLensService::new(None, false);
    let request = FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: active_document_path(&workspace),
        source: "import { missing } from 'missing-effectful-lib';".to_owned(),
        force_fresh: false,
        analysis_generation: None,
    };
    let stale = HashSet::from(["missing-effectful-lib".to_owned()]);

    let (_, _, results, _) = service
        .revalidate_document_sizes(&request, &stale, || true)
        .expect("a deterministic failure is durable, so it is cached and pushed");

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].sizes(),
        None,
        "the push may not carry a size the engine never measured: {results:?}",
    );
    assert_eq!(results[0].unmeasured_stage(), Some("missing_export"));
}

#[test]
fn revalidate_document_sizes_distinguishes_same_specifier_variants() {
    use std::collections::HashSet;

    // Two imports of the SAME package differing only by import kind / named exports
    // (default vs named). They share a specifier, so the push MUST carry a per-import
    // identity that distinguishes them — otherwise the client collapses them by
    // specifier and stamps one variant's size onto both (Finding 9).
    let workspace = temp_workspace();
    // Both a default AND a named export, so BOTH import variants resolve cleanly and
    // are cacheable: RB-13 now filters non-cacheable results (e.g. a default import of
    // a named-only package, which carries a missing-export diagnostic) off the SWR
    // push, so a valid default export is required for the Default variant to be pushed.
    write_tiny_package_with_source(
        &workspace,
        "export default 42;\nexport const value = 'tiny';",
    );
    let service = ImportLensService::new(None, false);
    let document = active_document_path(&workspace);
    let source = "import def from 'tiny-lib';\nimport { value } from 'tiny-lib';".to_owned();
    let request = FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document,
        source,
        force_fresh: false,
        analysis_generation: Some(7),
    };

    // Both variants share the specifier `tiny-lib`, so both are recomputed.
    let stale = HashSet::from(["tiny-lib".to_owned()]);
    let (_, _, results, identities) = service
        .revalidate_document_sizes(&request, &stale, || true)
        .expect("stale same-specifier variants should produce refreshed results");

    assert_eq!(
        results.len(),
        2,
        "both same-specifier variants recompute: {results:?}"
    );
    assert_eq!(
        identities.len(),
        results.len(),
        "identities align 1:1 with results"
    );
    assert!(
        identities
            .iter()
            .all(|identity| identity.specifier == "tiny-lib"),
        "both identities carry the shared specifier: {identities:?}"
    );
    // The identities differ by import kind, so the client can tell the variants apart
    // even though the specifier is identical.
    let has_default = identities
        .iter()
        .any(|identity| matches!(identity.import_kind, ImportKind::Default));
    let has_named = identities
        .iter()
        .any(|identity| matches!(identity.import_kind, ImportKind::Named));
    assert!(
        has_default && has_named,
        "the two variants are distinguished by import kind: {identities:?}"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[test]
fn service_analyzes_package_json_dependencies_in_daemon() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_analyze_package_json(AnalyzePackageJsonRequest {
        message_type: "analyze_package_json".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 34,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace.join("package.json").to_string_lossy().to_string(),
        source: r#"{
  "dependencies": { "tiny-lib": "^1.0.0", "missing-lib": "^1.0.0" },
  "devDependencies": { "ignored-object": { "version": "1.0.0" } }
}"#
        .to_owned(),
        include_registry_hints: false,
        force_registry_refresh: false,
        refresh_section: None,
        registry_hint_mode: None,
        streaming: false,
    });

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 34);
    assert_eq!(response.error, None);
    assert_eq!(response.sections.len(), 2);
    assert_eq!(response.states.len(), 2);
    let tiny = response
        .states
        .iter()
        .find(|state| state.name == "tiny-lib")
        .expect("tiny-lib state should exist");
    let missing = response
        .states
        .iter()
        .find(|state| state.name == "missing-lib")
        .expect("missing-lib state should exist");
    assert_eq!(tiny.status, ImportAnalysisStatus::Ready);
    assert_eq!(tiny.installed_version.as_deref(), Some("1.0.0"));
    assert_eq!(missing.status, ImportAnalysisStatus::Missing);
}

#[test]
fn service_streams_package_json_loading_states_before_ready_results() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);
    let partials = Mutex::new(Vec::new());

    let response = service.handle_analyze_package_json_streaming(
        AnalyzePackageJsonRequest {
            message_type: "analyze_package_json".to_owned(),
            version: PROTOCOL_VERSION,
            request_id: 36,
            workspace_root: workspace.to_string_lossy().to_string(),
            active_document_path: workspace.join("package.json").to_string_lossy().to_string(),
            source: r#"{
  "dependencies": { "tiny-lib": "^1.0.0", "missing-lib": "^1.0.0" }
}"#
            .to_owned(),
            include_registry_hints: false,
            force_registry_refresh: false,
            refresh_section: None,
            registry_hint_mode: None,
            streaming: true,
        },
        |partial| {
            partials
                .lock()
                .expect("partials lock should not be poisoned")
                .push(partial);
        },
    );

    let partials = partials
        .into_inner()
        .expect("partials lock should not be poisoned");

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 36);
    assert_eq!(response.error, None);
    assert_eq!(response.indexes, None);
    assert_eq!(response.states.len(), 2);
    assert!(
        response
            .states
            .iter()
            .any(|state| state.name == "tiny-lib" && state.status == ImportAnalysisStatus::Ready),
        "{response:?}",
    );
    assert!(
        response
            .states
            .iter()
            .any(|state| state.name == "missing-lib"
                && state.status == ImportAnalysisStatus::Missing),
        "{response:?}",
    );

    let initial = partials
        .first()
        .expect("initial package.json partial should be emitted");
    assert_eq!(initial.request_id, 36);
    assert_eq!(initial.indexes, Some(vec![0, 1]));
    assert_eq!(initial.states.len(), 2);
    assert!(initial.states.iter().all(|state| {
        state.status == ImportAnalysisStatus::Loading && state.installed_version.is_none()
    }));

    let resolved_loading = partials
        .iter()
        .skip(1)
        // Compare slice-to-slice (`as_slice`), not slice-to-array-ref: the array-ref form
        // (`Some(&[0, 1])`) fails inference once the wider dependency graph is in scope.
        .find(|partial| partial.indexes.as_deref() == Some([0usize, 1].as_slice()))
        .expect("resolved loading package.json partial should be emitted");
    assert!(
        resolved_loading
            .states
            .iter()
            .any(|state| state.name == "tiny-lib"
                && state.status == ImportAnalysisStatus::Loading
                && state.installed_version.as_deref() == Some("1.0.0")),
        "{resolved_loading:?}",
    );
    assert!(
        resolved_loading
            .states
            .iter()
            .any(|state| state.name == "missing-lib"
                && state.status == ImportAnalysisStatus::Missing),
        "{resolved_loading:?}",
    );
    let tiny_index = initial
        .states
        .iter()
        .position(|state| state.name == "tiny-lib")
        .expect("tiny-lib should be present in initial partial");
    assert!(
        partials.iter().any(|partial| {
            partial.indexes.as_deref() == Some([tiny_index].as_slice())
                && partial.states.first().is_some_and(|state| {
                    state.name == "tiny-lib" && state.status == ImportAnalysisStatus::Ready
                })
        }),
        "{partials:?}",
    );
}

#[test]
fn service_batches_cached_package_json_size_partials_before_uncached_work() {
    let workspace = temp_workspace();
    for package_name in ["cached-a", "cached-b", "cached-c", "uncached-d"] {
        write_named_package(&workspace, package_name);
    }
    let service = ImportLensService::new(None, false);
    let warm_source =
        r#"{"dependencies":{"cached-a":"^1.0.0","cached-b":"^1.0.0","cached-c":"^1.0.0"}}"#;
    let streamed_source = r#"{"dependencies":{"cached-a":"^1.0.0","cached-b":"^1.0.0","cached-c":"^1.0.0","uncached-d":"^1.0.0"}}"#;

    let warmup = service.handle_analyze_package_json(package_json_request(
        &workspace,
        37,
        warm_source,
        false,
    ));
    assert_eq!(warmup.error, None);
    assert_eq!(warmup.states.len(), 3);
    assert!(
        warmup
            .states
            .iter()
            .all(|state| state.status == ImportAnalysisStatus::Ready),
        "{warmup:?}",
    );

    let partials = Mutex::new(Vec::new());
    let response = service.handle_analyze_package_json_streaming(
        package_json_request(&workspace, 38, streamed_source, true),
        |partial| {
            partials
                .lock()
                .expect("partials lock should not be poisoned")
                .push(partial);
        },
    );
    let partials = partials
        .into_inner()
        .expect("partials lock should not be poisoned");

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None);
    assert_eq!(response.states.len(), 4);
    let cached_ready = partials
        .iter()
        .find(|partial| {
            partial.indexes.as_deref() == Some([0usize, 1, 2].as_slice())
                && partial.states.len() == 3
                && partial.states.iter().all(|state| {
                    state.status == ImportAnalysisStatus::Ready
                        && state.result.as_ref().is_some_and(|result| result.cache_hit)
                })
        })
        .expect("cached package sizes should stream as one indexed partial");
    assert_eq!(
        cached_ready
            .states
            .iter()
            .map(|state| state.name.as_str())
            .collect::<Vec<_>>(),
        vec!["cached-a", "cached-b", "cached-c"],
        "{cached_ready:?}",
    );
    assert!(
        partials.iter().any(|partial| {
            partial.indexes.as_deref() == Some([3usize].as_slice())
                && partial.states.first().is_some_and(|state| {
                    state.name == "uncached-d"
                        && state.status == ImportAnalysisStatus::Ready
                        && state
                            .result
                            .as_ref()
                            .is_some_and(|result| !result.cache_hit)
                })
        }),
        "{partials:?}",
    );
}

#[test]
fn service_completes_import_members_from_document_context() {
    let workspace = temp_workspace();
    write_export_package(&workspace);
    let service = ImportLensService::new(None, false);
    let source = "import { local } from 'exports-lib';";
    let cursor_offset = source.find("local").expect("member should exist");

    let response = service.complete_import_members(CompleteImportMembersRequest {
        message_type: "complete_import_members".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 35,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: active_document_path(&workspace),
        source: source.to_owned(),
        cursor_offset,
    });

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.request_id, 35);
    assert_eq!(response.error, None);
    assert_eq!(response.specifier.as_deref(), Some("exports-lib"));
    assert_eq!(response.imported_names, vec!["local"]);
    assert_eq!(response.exports, vec!["beta", "local", "renamed"]);
}

#[test]
fn service_invalidates_packages_from_node_modules_package_json_paths() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let _ = service.handle_batch(batch(&workspace, 1));
    let invalidated = service.invalidate_package_json_paths(&[workspace
        .join("node_modules")
        .join("tiny-lib")
        .join("package.json")
        .to_string_lossy()
        .to_string()]);
    let after_invalidate = service.handle_batch(batch(&workspace, 2));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(invalidated);
    assert!(!after_invalidate.imports[0].cache_hit);
}

#[test]
fn service_bulk_invalidation_is_scoped_to_changed_packages() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);

    let _ = service.handle_batch(batch(&workspace, 1));

    // A large burst of UNRELATED package.json changes must not evict tiny-lib:
    // bulk invalidation is scoped to the named packages, never a workspace nuke.
    let unrelated_paths = (0..21)
        .map(|index| {
            workspace
                .join("node_modules")
                .join(format!("package-{index}"))
                .join("package.json")
                .to_string_lossy()
                .to_string()
        })
        .collect::<Vec<_>>();
    let invalidated_unrelated = service.invalidate_package_json_paths(&unrelated_paths);
    let after_unrelated = service.handle_batch(batch(&workspace, 2));

    // Invalidating tiny-lib itself does evict it.
    let tiny_lib_path = workspace
        .join("node_modules")
        .join("tiny-lib")
        .join("package.json")
        .to_string_lossy()
        .to_string();
    let invalidated_tiny = service.invalidate_package_json_paths(&[tiny_lib_path]);
    let after_tiny = service.handle_batch(batch(&workspace, 3));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(invalidated_unrelated);
    assert!(
        after_unrelated.imports[0].cache_hit,
        "unrelated bulk invalidation must leave tiny-lib cached"
    );
    assert!(invalidated_tiny);
    assert!(
        !after_tiny.imports[0].cache_hit,
        "invalidating tiny-lib itself must evict it"
    );
}

#[test]
fn service_skips_unmappable_package_json_paths_and_targets_only_mappable_ones() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
    let workspace = temp_workspace();
    write_package(&workspace);
    write_named_package(&workspace, "other-lib");
    let service = ImportLensService::new(None, false);

    let _ = service.handle_batch(batch(&workspace, 1));
    let _ = service.handle_batch(package_batch(&workspace, 2, "other-lib", "value"));

    let tiny_lib_path = workspace
        .join("node_modules")
        .join("tiny-lib")
        .join("package.json")
        .to_string_lossy()
        .to_string();
    // No "/node_modules/" segment anywhere in this path, so
    // `package_name_from_package_json_path` returns `None` for it -- the same
    // shape an unusual layout (pnpm's `.pnpm/…` store, a symlinked package, or
    // any structure the mapper doesn't recognize) would produce on a routine
    // install.
    let unmappable_path = workspace.join("package.json").to_string_lossy().to_string();

    let invalidated = service.invalidate_package_json_paths(&[tiny_lib_path, unmappable_path]);

    let after_tiny = service.handle_batch(batch(&workspace, 3));
    let after_other = service.handle_batch(package_batch(&workspace, 4, "other-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(invalidated);
    assert!(
        !after_tiny.imports[0].cache_hit,
        "the mappable path's package must still be invalidated"
    );
    assert!(
        after_other.imports[0].cache_hit,
        "an unrelated cached package must survive -- one unmappable path must not trigger invalidate_all"
    );
}

#[test]
fn service_invalidates_all_when_every_package_json_path_is_unmappable() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
    let workspace = temp_workspace();
    write_package(&workspace);
    write_named_package(&workspace, "other-lib");
    let service = ImportLensService::new(None, false);

    let _ = service.handle_batch(batch(&workspace, 1));
    let _ = service.handle_batch(package_batch(&workspace, 2, "other-lib", "value"));

    let unmappable_path = workspace.join("package.json").to_string_lossy().to_string();

    let invalidated = service.invalidate_package_json_paths(&[unmappable_path]);

    let after_tiny = service.handle_batch(batch(&workspace, 3));
    let after_other = service.handle_batch(package_batch(&workspace, 4, "other-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(invalidated);
    assert!(
        !after_tiny.imports[0].cache_hit,
        "a wholly unmappable, non-empty batch must fall back to invalidate_all"
    );
    assert!(
        !after_other.imports[0].cache_hit,
        "a wholly unmappable, non-empty batch must fall back to invalidate_all"
    );
}

#[test]
fn node_modules_change_reclaims_uninstalled_package_entries() {
    // §7 / B5: an UNINSTALLED package's cached entries are reclaimed AUTOMATICALLY by
    // the `NodeModulesChanged` event (targeted name invalidation) -- no orphan scan and
    // no user action. `invalidate_package_json_paths` maps the package name from the
    // manifest PATH STRING, so it still targets the right package after the manifest is
    // gone (it never stats or reads the deleted file). A sibling package that is still
    // installed must survive, proving the reclaim is targeted, not a workspace nuke.
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
    let workspace = temp_workspace();
    write_package(&workspace); // node_modules/tiny-lib
    write_named_package(&workspace, "other-lib");
    let service = ImportLensService::new(None, false);

    // Cache both packages.
    let _ = service.handle_batch(batch(&workspace, 1));
    let _ = service.handle_batch(package_batch(&workspace, 2, "other-lib", "value"));

    // Uninstall tiny-lib entirely (manifest + entry gone); other-lib stays installed.
    fs::remove_dir_all(workspace.join("node_modules").join("tiny-lib"))
        .expect("tiny-lib should be uninstallable");

    // The extension observes the node_modules mutation and fires `NodeModulesChanged`
    // with the (now-removed) tiny-lib manifest path -- the automatic reclaim path.
    let manifest_path = workspace
        .join("node_modules")
        .join("tiny-lib")
        .join("package.json")
        .to_string_lossy()
        .to_string();
    let invalidated = service.invalidate_package_json_paths(&[manifest_path]);

    let after_tiny = service.handle_batch(batch(&workspace, 3));
    let after_other = service.handle_batch(package_batch(&workspace, 4, "other-lib", "value"));

    fs::remove_dir_all(&workspace).ok();
    assert!(
        invalidated,
        "a mappable manifest path must invalidate by name even after the package is \
         uninstalled -- the name comes from the path string, never a stat of the file"
    );
    assert!(
        !after_tiny.imports[0].cache_hit,
        "the uninstalled package's entries must be reclaimed automatically (no orphan scan)"
    );
    assert!(
        after_other.imports[0].cache_hit,
        "a still-installed sibling must survive -- the reclaim is targeted, not a nuke"
    );
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
    assert_ne!(left.imports[0].raw_bytes(), right.imports[0].raw_bytes());
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
    assert_ne!(
        component.imports[0].raw_bytes(),
        server.imports[0].raw_bytes()
    );
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
fn service_reports_and_removes_per_project_cache_shards() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
    let storage = common::temp_workspace("import-lens-service-cache-storage");
    let left_workspace = temp_workspace();
    let right_workspace = temp_workspace();
    write_package(&left_workspace);
    write_tiny_package_with_source(
        &right_workspace,
        "export const value = 'right workspace has different package bytes';",
    );
    let service = ImportLensService::new_with_cache_policy(Some(storage.clone()), true, 512, 32);

    let _ = service.handle_batch(batch(&left_workspace, 1));
    let _ = service.handle_batch(batch(&right_workspace, 2));

    let status = service.cache_status(CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 3,
        workspace_root: Some(left_workspace.to_string_lossy().to_string()),
    });
    assert_eq!(status.project_count, 2);
    assert_eq!(
        status
            .current_project
            .as_ref()
            .map(|shard| shard.project_root.as_str()),
        Some(left_workspace.to_string_lossy().as_ref())
    );

    let removed = service.remove_cache(CacheRemoveRequest {
        message_type: "cache_remove".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 4,
        scope: CacheRemoveScope::CurrentProject,
        workspace_root: Some(left_workspace.to_string_lossy().to_string()),
        shard_ids: None,
    });
    assert_eq!(removed.removed.len(), 1);
    assert!(removed.failed.is_empty(), "{removed:?}");

    let after_remove = service.cache_status(CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 5,
        workspace_root: Some(left_workspace.to_string_lossy().to_string()),
    });
    assert_eq!(after_remove.project_count, 1);
    assert!(after_remove.current_project.is_none());

    fs::remove_dir_all(left_workspace).expect("left workspace should be removed");
    fs::remove_dir_all(right_workspace).expect("right workspace should be removed");
    fs::remove_dir_all(storage).expect("cache storage should be removed");
}

#[test]
fn cache_status_reports_total_budget_registry_and_per_project_counts() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
    let storage = common::temp_workspace("import-lens-service-cache-status-observability");
    let left_workspace = temp_workspace();
    let right_workspace = temp_workspace();
    write_package(&left_workspace);
    write_tiny_package_with_source(
        &right_workspace,
        "export const value = 'right workspace has different package bytes';",
    );
    let max_size_mb = 512u64;
    let service =
        ImportLensService::new_with_cache_policy(Some(storage.clone()), true, max_size_mb, 32);

    // Two analyses seed two disk shards, each holding at least one entry.
    let _ = service.handle_batch(batch(&left_workspace, 1));
    let _ = service.handle_batch(batch(&right_workspace, 2));

    // Baseline registry size BEFORE seeding a hint: the empty snapshot still
    // serializes to a small non-zero envelope, so seeding must grow it.
    let before = service.cache_status(CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 3,
        workspace_root: Some(left_workspace.to_string_lossy().to_string()),
    });

    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("observability-pkg", "1.0.0", 100);

    let status = service.cache_status(CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 4,
        workspace_root: Some(left_workspace.to_string_lossy().to_string()),
    });

    assert_eq!(status.project_count, 2);
    // `budget_bytes` is the configured cache size expressed in bytes.
    assert_eq!(status.budget_bytes, max_size_mb * 1024 * 1024);
    // `total_bytes` sums every shard's logical (envelope) bytes from the C1
    // rollup; both seeded shards hold entries, so it is non-zero and can never
    // exceed the physical on-disk footprint.
    assert!(status.total_bytes > 0, "{status:?}");
    assert!(status.total_bytes <= status.total_size_bytes, "{status:?}");
    // Seeding a registry hint grows the serialized registry snapshot.
    assert!(status.registry_size_bytes > 0, "{status:?}");
    assert!(
        status.registry_size_bytes > before.registry_size_bytes,
        "seeding a registry hint must grow registry_size_bytes: {before:?} -> {status:?}"
    );
    // The current project's per-project entry count comes from the O(1) rollup.
    let current = status
        .current_project
        .as_ref()
        .expect("left workspace shard should be the current project");
    assert!(current.entry_count >= 1, "{current:?}");

    fs::remove_dir_all(left_workspace).expect("left workspace should be removed");
    fs::remove_dir_all(right_workspace).expect("right workspace should be removed");
    fs::remove_dir_all(storage).expect("cache storage should be removed");
}

#[test]
fn user_clear_bumps_generation_so_inflight_insert_revalidates() {
    let service = ImportLensService::new(None, false);
    let before = import_lens_daemon::cache::memory::cache_generation();

    let response = service.remove_cache(CacheRemoveRequest {
        message_type: "cache_remove".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        scope: CacheRemoveScope::All,
        workspace_root: None,
        shard_ids: None,
    });

    assert!(response.error.is_none(), "{response:?}");
    assert!(
        import_lens_daemon::cache::memory::cache_generation() > before,
        "every mutating clear scope must bump the generation (X-17)"
    );
}

#[test]
fn user_clear_bumps_generation_for_current_project_and_selected_scopes() {
    let workspace = temp_workspace();

    // Both cases have nothing to actually remove (no shard was ever cached for
    // this workspace; the selected-shard list is empty) -- the bump must still
    // fire unconditionally for any non-error scope (X-17).
    for (scope, workspace_root, shard_ids) in [
        (
            CacheRemoveScope::CurrentProject,
            Some(workspace.to_string_lossy().to_string()),
            None,
        ),
        (CacheRemoveScope::Selected, None, Some(Vec::new())),
    ] {
        let service = ImportLensService::new(None, false);
        let before = import_lens_daemon::cache::memory::cache_generation();

        let response = service.remove_cache(CacheRemoveRequest {
            message_type: "cache_remove".to_owned(),
            version: PROTOCOL_VERSION,
            request_id: 1,
            scope,
            workspace_root,
            shard_ids,
        });

        assert!(response.error.is_none(), "{response:?}");
        assert!(
            import_lens_daemon::cache::memory::cache_generation() > before,
            "scope must bump the generation even when nothing matched (X-17)"
        );
    }

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

#[test]
fn remove_registry_scope_clears_only_registry() {
    let storage = common::temp_workspace("import-lens-registry-scope-storage");
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new_with_cache_policy(Some(storage.clone()), true, 512, 32);

    // Seed BOTH a bundle shard (a real analysis) and a registry hint.
    let _ = service.handle_batch(batch(&workspace, 1));
    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("registry-scope-pkg", "1.0.0", 100);

    let target = RegistryHintTarget {
        name: "registry-scope-pkg".to_owned(),
        installed_version: None,
    };
    assert!(
        service
            .refresh_registry_hint_target(target.clone(), RegistryHintMode::Cached, 100)
            .hint
            .is_some(),
        "registry hint should be seeded before the clear"
    );
    let seeded_status = service.cache_status(CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 2,
        workspace_root: Some(workspace.to_string_lossy().to_string()),
    });
    assert_eq!(
        seeded_status.project_count, 1,
        "bundle shard should be seeded before the clear"
    );

    let before_gen = import_lens_daemon::cache::memory::cache_generation();

    let response = service.remove_cache(CacheRemoveRequest {
        message_type: "cache_remove".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 3,
        scope: CacheRemoveScope::Registry,
        workspace_root: None,
        shard_ids: None,
    });

    // A registry-only clear reports no shard removals and no failures (honest:
    // it must not claim bundle-shard removals it never made).
    assert!(response.error.is_none(), "{response:?}");
    assert!(
        response.removed.is_empty() && response.failed.is_empty(),
        "Registry scope must not touch bundle shards: {response:?}"
    );

    // The registry hint is gone...
    assert!(
        service
            .refresh_registry_hint_target(target, RegistryHintMode::Cached, 100)
            .hint
            .is_none(),
        "Registry scope must clear the npm-hint store"
    );

    // ...but the bundle shard SURVIVES.
    let after_status = service.cache_status(CacheStatusRequest {
        message_type: "cache_status".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 4,
        workspace_root: Some(workspace.to_string_lossy().to_string()),
    });
    assert_eq!(
        after_status.project_count, 1,
        "Registry scope must leave bundle shards intact"
    );

    // Every mutating scope bumps the generation (X-17).
    assert!(
        import_lens_daemon::cache::memory::cache_generation() > before_gen,
        "Registry scope must bump the cache generation"
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    fs::remove_dir_all(&storage).expect("cache storage should be removed");
}

#[test]
fn remove_all_clears_registry_resolvers_and_l1_even_when_no_shard_removed() {
    let _shared_index_guard = SHARED_INDEX_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");

    // No disk bundle cache -> `remove_all` removes zero shards, exercising the
    // X-21 "no shard removed" path where the derived caches must still be
    // dropped unconditionally.
    let service =
        ImportLensService::new_with_registry_hints_for_tests(RegistryHintService::disabled());

    // Seed the registry hint store (observable via a cached-mode lookup).
    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("clear-all-pkg", "1.0.0", 100);
    let target = RegistryHintTarget {
        name: "clear-all-pkg".to_owned(),
        installed_version: None,
    };
    assert!(
        service
            .refresh_registry_hint_target(target.clone(), RegistryHintMode::Cached, 100)
            .hint
            .is_some(),
        "registry hint should be seeded before the clear"
    );

    // Seed L1 (file-size aggregate) under a unique document path.
    let l1_workspace = temp_workspace();
    let l1_path = l1_workspace.join("src").join("l1-doc.ts");
    shared_file_size_cache().insert(l1_path.clone(), 1, FileSizeComputation::default());
    assert!(
        shared_file_size_cache().contains_path(&l1_path),
        "L1 aggregate should be seeded before the clear"
    );

    let before_resolvers = shared_resolvers();
    let before_gen = import_lens_daemon::cache::memory::cache_generation();

    let response = service.remove_cache(CacheRemoveRequest {
        message_type: "cache_remove".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        scope: CacheRemoveScope::All,
        workspace_root: None,
        shard_ids: None,
    });

    assert!(response.error.is_none(), "{response:?}");
    assert!(
        response.removed.is_empty(),
        "no shard should have been removed -- the X-21 condition: {response:?}"
    );

    // Registry hint store cleared (X-14).
    assert!(
        service
            .refresh_registry_hint_target(target, RegistryHintMode::Cached, 100)
            .hint
            .is_none(),
        "All must clear the registry hint store"
    );
    // Shared resolvers invalidated -- a fresh ResolverSet was published (X-16).
    assert!(
        !Arc::ptr_eq(&before_resolvers, &shared_resolvers()),
        "All must invalidate the shared resolvers"
    );
    // L1 aggregate cache cleared even though no shard was removed (X-21).
    assert!(
        !shared_file_size_cache().contains_path(&l1_path),
        "All must clear the L1 file-size cache even when no shard was removed"
    );
    // Generation bumped (X-17).
    assert!(
        import_lens_daemon::cache::memory::cache_generation() > before_gen,
        "All must bump the cache generation"
    );

    fs::remove_dir_all(&l1_workspace).expect("l1 workspace should be removed");
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
    // A node_modules change bumps the cache generation in production (the
    // extension sends node_modules_changed); that forces the fingerprint
    // re-verification which detects the changed dependency.
    import_lens_daemon::cache::memory::bump_cache_generation();
    let second = service.handle_batch(package_batch(&workspace, 2, "dependent-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert!(!second.imports[0].cache_hit);
    assert_ne!(first.imports[0].raw_bytes(), second.imports[0].raw_bytes());
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
    // A node_modules change bumps the cache generation in production (the
    // extension sends node_modules_changed); that forces the fingerprint
    // re-verification which detects the changed transitive dependency.
    import_lens_daemon::cache::memory::bump_cache_generation();
    let second = service.handle_batch(package_batch(&workspace, 2, "parent-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert!(!second.imports[0].cache_hit);
    assert_ne!(first.imports[0].raw_bytes(), second.imports[0].raw_bytes());
}

/// A manifest the resolver cannot use leaves the import with no cache KEY — the key is derived
/// from the resolved package — so it is re-analyzed every time and never cached. That has not
/// changed. What has is the answer: it used to be the package directory's bytes on disk, dressed
/// up as five compressed sizes. It is Unmeasured now (ADR-0006 §1).
#[test]
fn service_reports_an_unusable_manifest_as_unmeasured_and_caches_nothing() {
    let workspace = temp_workspace();
    write_versionless_package(&workspace);
    let service = ImportLensService::new(None, false);
    let request = package_batch(&workspace, 1, "versionless-lib", "value");
    let mut second_request = request.clone();
    second_request.request_id = 2;

    let first = service.handle_batch(request);
    let second = service.handle_batch(second_request);

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert!(!second.imports[0].cache_hit);
    assert_eq!(first.imports[0].sizes(), None, "{first:?}");
    assert_eq!(
        first.imports[0].unmeasured_stage(),
        Some("package_manifest"),
        "{first:?}",
    );
}

// The pre-cutover engine aliased a Named result into the Namespace cache key
// for side-effectful packages because it sized both identically. Rolldown
// still shakes pure unused exports under `sideEffects: true`, so the sizes
// legitimately differ and each import kind must compute its own entry.
#[test]
fn service_does_not_alias_named_results_to_namespace_cache() {
    let workspace = temp_workspace();
    write_effectful_package(&workspace);
    let service = ImportLensService::new(None, false);

    let named = service.handle_batch(effectful_batch(&workspace, 1, ImportKind::Named));
    let namespace = service.handle_batch(effectful_batch(&workspace, 2, ImportKind::Namespace));
    let namespace_again =
        service.handle_batch(effectful_batch(&workspace, 3, ImportKind::Namespace));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(!named.imports[0].cache_hit);
    assert!(!namespace.imports[0].cache_hit);
    assert!(namespace_again.imports[0].cache_hit);
    assert!(namespace.imports[0].raw_bytes() >= named.imports[0].raw_bytes());
}

#[test]
fn service_caches_a_deterministic_missing_export_failure() {
    let workspace = temp_workspace();
    write_missing_export_effectful_package(&workspace);
    let service = ImportLensService::new(None, false);

    let first = service.handle_batch(missing_effectful_batch(&workspace, 1, ImportKind::Named));
    // The SAME request again. A `missing_export` failure is a property of the package's bytes: it
    // will fail identically every time, so it is cached (ADR-0006, invariant 3) and this second
    // request must be answered without re-entering the engine.
    let second = service.handle_batch(missing_effectful_batch(&workspace, 2, ImportKind::Named));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert_eq!(first.imports[0].sizes(), None, "{first:?}");
    assert_eq!(
        first.imports[0].unmeasured_stage(),
        Some("missing_export"),
        "{first:?}",
    );
    assert!(
        second.imports[0].cache_hit,
        "a deterministic failure is cached; refusing it would re-enter the engine for a broken \
         package on every analysis, forever, on one of only two permits: {second:?}",
    );
    assert_eq!(second.imports[0].sizes(), None, "{second:?}");
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

/// On a cold document EVERY import arrives by push, so if the streamed builds do not annotate
/// shared bytes, the shared-dependency insight silently never appears on a first analysis —
/// exactly the documents the user just opened.
///
/// It cannot be annotated per push: `shared_bytes` is a relation between two imports of the same
/// file, so it is not knowable until the last one has been measured. Each import is therefore
/// delivered the moment its own number lands, and the stream closes with one correction push
/// carrying the shared figures for the whole document.
#[test]
fn streamed_imports_get_their_shared_bytes_when_the_document_closes() {
    use import_lens_daemon::document::IgnoreRuleResolver;
    use import_lens_daemon::pipeline::analyze::AnalysisContext;
    use std::collections::HashMap;

    let workspace = temp_workspace();
    write_shared_packages(&workspace);
    let service = ImportLensService::new(None, false);
    let source = "import { left } from 'left-lib';\nimport { right } from 'right-lib';";

    let analysis = service.handle_analyze_document_streaming(
        AnalyzeDocumentRequest {
            message_type: "analyze_document".to_owned(),
            version: PROTOCOL_VERSION,
            request_id: 71,
            workspace_root: workspace.to_string_lossy().to_string(),
            active_document_path: active_document_path(&workspace),
            source: source.to_owned(),
        },
        &IgnoreRuleResolver::default(),
    );
    assert_eq!(
        analysis.pending.len(),
        2,
        "a cold document defers every import"
    );

    let context = AnalysisContext {
        workspace_root: PathBuf::from(workspace.to_string_lossy().to_string()),
        active_document_path: PathBuf::from(active_document_path(&workspace)),
    };
    let pushed = Mutex::new(Vec::new());
    service.complete_pending_imports(
        &context,
        analysis.measured,
        analysis.pending,
        || true,
        |results, identities| {
            pushed
                .lock()
                .expect("pushes")
                .extend(identities.into_iter().zip(results));
        },
    );

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    // Later pushes supersede earlier ones for the same import, exactly as the client's merge does.
    let delivered = pushed
        .into_inner()
        .expect("pushes")
        .into_iter()
        .map(|(identity, result)| (identity.specifier, result.shared_bytes))
        .collect::<HashMap<_, _>>();

    for specifier in ["left-lib", "right-lib"] {
        assert!(
            delivered
                .get(specifier)
                .copied()
                .flatten()
                .is_some_and(|bytes| bytes > 0),
            "{specifier} must end up carrying the bytes it shares with its sibling: {delivered:?}"
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
        .filter_map(|result| result.raw_bytes())
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
fn service_computes_file_size_for_commonjs_imports() {
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
    assert!(file_size.imports[0].is_cjs, "{file_size:?}");
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
    assert_eq!(
        response.imports[0].sizes(),
        None,
        "a request rejected before any build ran has no size — not a zero"
    );
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

#[test]
fn package_json_analysis_includes_cached_registry_hints_when_requested() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new_with_cache_policy(None, false, 512, 32);
    service
        .registry_hints_for_tests()
        .write_metadata_for_tests("tiny-lib", "1.1.0", 100);

    let response = service.handle_analyze_package_json(AnalyzePackageJsonRequest {
        message_type: "analyze_package_json".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 40,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace.join("package.json").to_string_lossy().to_string(),
        source: r#"{"dependencies":{"tiny-lib":"^1.0.0"}}"#.to_owned(),
        streaming: false,
        include_registry_hints: true,
        force_registry_refresh: false,
        refresh_section: None,
        registry_hint_mode: Some(import_lens_daemon::ipc::protocol::RegistryHintMode::Cached),
    });

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None);
    assert_eq!(
        response.states[0]
            .registry_hint
            .as_ref()
            .and_then(|hint| hint.latest_version.as_deref()),
        Some("1.1.0")
    );
}

/// A cached DETERMINISTIC failure must expire against the bytes it was derived from — the whole
/// graph, not just the entry the caller named.
///
/// ADR-0006 widened the cache to take a deterministic failure, on the reasoning that it is a
/// property of the package's bytes and "the cache is keyed by those bytes' fingerprints, so it
/// expires exactly when the answer would change". That promise is only true if the failure is
/// fingerprinted against the graph. A first-party workspace package whose entry merely RE-EXPORTS
/// the module that fails to parse would otherwise serve the cached failure forever after the user
/// fixed it: nothing the cache was watching moved. The engine reports what it had loaded when it
/// gave up, and that is what the failure is keyed on.
#[test]
fn a_cached_deterministic_failure_expires_when_the_module_that_caused_it_is_fixed() {
    let workspace = temp_workspace();
    let package_root = workspace.join("node_modules").join("broken-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest should be written");
    // The entry is fine. The module it re-exports is not — so a fingerprint set built from the
    // entry and the manifest alone would never notice the fix below.
    fs::write(
        package_root.join("index.js"),
        "export { value } from './broken.js';\n",
    )
    .expect("entry should be written");
    fs::write(package_root.join("broken.js"), "export const value = ;\n")
        .expect("broken module should be written");

    let service = ImportLensService::new(None, false);
    let broken = service.handle_batch(package_batch(&workspace, 1, "broken-lib", "value"));

    assert_eq!(
        broken.imports[0].sizes(),
        None,
        "the premise: a package whose transitive module cannot be parsed has no size",
    );
    assert_eq!(broken.imports[0].unmeasured_stage(), Some("parse"));

    // The user fixes the module the entry re-exports. The entry itself never changes.
    fs::write(package_root.join("broken.js"), "export const value = 1;\n")
        .expect("fixed module should be written");

    let fixed = service.handle_batch(package_batch(&workspace, 2, "broken-lib", "value"));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(
        fixed.imports[0].sizes().is_some(),
        "the cached failure must expire against the module that caused it, not the entry: {fixed:?}",
    );
}

/// A failed CSS `@import` child is an input to the cached fallback just as surely as a parsed
/// JavaScript dependency is an input to a cached engine failure. Losing the provider's observations
/// on `Err` leaves the child outside freshness, so fixing only that file keeps serving the old
/// `uncounted_assets` result.
#[test]
fn fixing_only_a_broken_css_import_child_invalidates_the_cached_fallback() {
    let workspace = temp_workspace();
    let package_root = workspace.join("node_modules").join("broken-css-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.css"]}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './styles.css';\nexport const value = 1;\n",
    )
    .expect("entry should be written");
    fs::write(
        package_root.join("styles.css"),
        "@import './broken.css';\n.entry { color: blue; }\n",
    )
    .expect("stylesheet should be written");
    fs::write(
        package_root.join("broken.css"),
        "$brand: red;\n@mixin thing { color: $brand }\n.bad { @include thing }\n",
    )
    .expect("broken child should be written");

    let service = ImportLensService::new(None, false);
    let broken = service.handle_batch(package_batch(&workspace, 1, "broken-css-lib", "value"));
    assert!(
        broken.imports[0].asset_breakdown.is_empty(),
        "the premise: the broken stylesheet cannot be counted: {broken:?}"
    );
    assert!(
        broken.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"),
        "the fallback must be disclosed: {broken:?}"
    );

    // Only the imported child changes; the JavaScript entry and top-level stylesheet stay put.
    fs::write(
        package_root.join("broken.css"),
        ".fixed { color: rebeccapurple; padding: 12345px; }\n",
    )
    .expect("child should be fixed");
    let fixed = service.handle_batch(package_batch(&workspace, 2, "broken-css-lib", "value"));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(
        fixed.imports[0]
            .asset_breakdown
            .iter()
            .any(|contribution| contribution.kind == import_lens_daemon::engine::AssetKind::Css),
        "fixing the child must invalidate the cached fallback and count CSS: {fixed:?}"
    );
    assert!(
        !fixed.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"),
        "the stale fallback disclosure must disappear after the fix: {fixed:?}"
    );
}

#[test]
fn creating_a_missing_css_import_child_invalidates_the_cached_fallback() {
    let workspace = temp_workspace();
    let package_root = workspace.join("node_modules").join("missing-css-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["*.css"]}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './styles.css';\nexport const value = 1;\n",
    )
    .expect("entry should be written");
    fs::write(
        package_root.join("styles.css"),
        "@import './created-later.css';\n.entry { color: blue; }\n",
    )
    .expect("stylesheet should be written");

    let service = ImportLensService::new(None, false);
    let missing = service.handle_batch(package_batch(&workspace, 1, "missing-css-lib", "value"));
    assert!(
        missing.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"),
        "the missing child must produce the disclosed CSS fallback: {missing:?}"
    );
    assert!(
        missing.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "asset_io"),
        "the wire result must say that this fallback came from a non-durable filesystem failure: \
         {missing:?}"
    );

    fs::write(
        package_root.join("created-later.css"),
        ".created { color: green; padding: 9876px; }\n",
    )
    .expect("missing child should be created");
    let created = service.handle_batch(package_batch(&workspace, 2, "missing-css-lib", "value"));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(
        created.imports[0]
            .asset_breakdown
            .iter()
            .any(|contribution| contribution.kind == import_lens_daemon::engine::AssetKind::Css),
        "creating the previously missing child must force a fresh CSS measurement: {created:?}"
    );
    assert!(
        !created.imports[0].cache_hit,
        "an unreadable input must never make its fallback look fresh: {created:?}"
    );
}

/// A direct asset is read in the engine plugin before Rolldown sees it. If that read fails after
/// metadata succeeded, handing the path back without retaining the failed observation lets the
/// resulting fallback/failure cache against only the manifest and JavaScript entry. The read can
/// recover without either changing, so that machine-dependent outcome must never become durable.
#[test]
fn a_direct_asset_read_failure_does_not_enter_the_import_cache() {
    let workspace = temp_workspace();
    let package_root = workspace
        .join("node_modules")
        .join("temporarily-unreadable-asset-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './font.woff2';\nexport const value = 1;\n",
    )
    .expect("entry should be written");

    // A directory has readable metadata but cannot be read as file bytes on every supported OS.
    // It deterministically exercises the stat-succeeds/read-fails race without permissions or an
    // antivirus-specific test hook.
    let font = package_root.join("font.woff2");
    fs::create_dir(&font).expect("unreadable-as-a-file asset should be created");

    let service = ImportLensService::new(None, false);
    let first = service.handle_batch(package_batch(
        &workspace,
        1,
        "temporarily-unreadable-asset-lib",
        "value",
    ));
    let repeated = service.handle_batch(package_batch(
        &workspace,
        2,
        "temporarily-unreadable-asset-lib",
        "value",
    ));
    assert!(!first.imports[0].cache_hit, "{first:?}");
    assert!(
        !repeated.imports[0].cache_hit,
        "a filesystem read failure describes this attempt, not package bytes: {repeated:?}"
    );
    assert!(
        first.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "asset_io"),
        "the private failed-read fingerprint must also have a wire-visible durability signal: \
         {first:?}"
    );

    fs::remove_dir(&font).expect("temporary directory asset should be removed");
    fs::write(&font, [0x51; 64]).expect("readable font should replace it");
    let recovered = service.handle_batch(package_batch(
        &workspace,
        3,
        "temporarily-unreadable-asset-lib",
        "value",
    ));
    let healthy_repeat = service.handle_batch(package_batch(
        &workspace,
        4,
        "temporarily-unreadable-asset-lib",
        "value",
    ));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert!(!recovered.imports[0].cache_hit, "{recovered:?}");
    assert!(
        recovered.imports[0]
            .asset_breakdown
            .iter()
            .any(|asset| asset.kind == import_lens_daemon::engine::AssetKind::Font),
        "fixing only the failed asset read must recover immediately: {recovered:?}"
    );
    assert!(
        healthy_repeat.imports[0].cache_hit,
        "the regression must not pass by disabling this import's cache entirely: {healthy_repeat:?}"
    );
}

/// Recording every asset-looking resolve candidate would poison a deterministic failure caused by
/// some other module: the healthy asset has exact bytes and does not make the missing JavaScript
/// dependency transient. Only an asset path whose own resolution/read failed may do that.
#[test]
fn a_healthy_asset_does_not_make_an_unrelated_resolve_failure_non_cacheable() {
    let workspace = temp_workspace();
    let package_root = workspace
        .join("node_modules")
        .join("healthy-asset-broken-js-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './font.woff2';\nimport './missing.js';\nexport const value = 1;\n",
    )
    .expect("entry should be written");
    fs::write(package_root.join("font.woff2"), [0x51; 64]).expect("healthy font should be written");

    let service = ImportLensService::new(None, false);
    let first = service.handle_batch(package_batch(
        &workspace,
        1,
        "healthy-asset-broken-js-lib",
        "value",
    ));
    let repeated = service.handle_batch(package_batch(
        &workspace,
        2,
        "healthy-asset-broken-js-lib",
        "value",
    ));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    assert_eq!(first.imports[0].unmeasured_stage(), Some("resolve"));
    assert!(
        repeated.imports[0].cache_hit,
        "a healthy asset must not make an unrelated deterministic JS resolve failure transient: \
         {repeated:?}"
    );
    assert!(
        !repeated.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "asset_io"),
        "only an actual asset failure earns the asset_io stage: {repeated:?}"
    );
}

#[test]
fn direct_asset_observation_preserves_the_client_browser_alias() {
    let workspace = temp_workspace();
    let package_root = workspace
        .join("node_modules")
        .join("browser-aliased-asset-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":true,"browser":{"./font.woff2":"./font-browser.woff2"}}"#,
    )
    .expect("manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "import './font.woff2';\nexport const value = 1;\n",
    )
    .expect("entry should be written");
    fs::write(package_root.join("font.woff2"), [0x11; 64]).expect("server font should be written");
    fs::write(package_root.join("font-browser.woff2"), [0x22; 4096])
        .expect("browser font should be written");

    let service = ImportLensService::new(None, false);
    let response = service.handle_batch(package_batch(
        &workspace,
        1,
        "browser-aliased-asset-lib",
        "value",
    ));

    fs::remove_dir_all(&workspace).expect("temp workspace should be removed");
    let font = response.imports[0]
        .asset_breakdown
        .iter()
        .find(|asset| asset.kind == import_lens_daemon::engine::AssetKind::Font)
        .expect("the browser-mapped font should be measured");
    assert_eq!(
        font.raw_bytes, 4096,
        "the observing hook must delegate to the configured browser-aware resolver: {response:?}"
    );
}
