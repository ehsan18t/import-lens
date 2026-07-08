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
    pipeline::graph::{
        build_module_graph_cached, clear_module_graph_cache, peek_cached_module_paths,
    },
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

static GRAPH_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

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

fn write_array_graph_effects_package(workspace: &Path) {
    let package_root = workspace
        .join("node_modules")
        .join("array-graph-effects-lib");
    fs::create_dir_all(package_root.join("dist").join("polyfill").join("browser"))
        .expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":["**/polyfill/**/*.js"]}"#,
    )
    .expect("package manifest should be written");
    fs::write(
        package_root.join("index.js"),
        "export const value = 1;\nexport { setup } from './dist/polyfill/browser/setup.js';",
    )
    .expect("entry should be written");
    fs::write(
        package_root
            .join("dist")
            .join("polyfill")
            .join("browser")
            .join("setup.js"),
        "globalThis.__importLensSideEffect = 'kept';\nexport const setup = 'polyfill';",
    )
    .expect("side-effect module should be written");
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

fn graph_effects_batch(workspace: &Path, request_id: u64, import_kind: ImportKind) -> BatchRequest {
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
            specifier: "array-graph-effects-lib".to_owned(),
            package_name: "array-graph-effects-lib".to_owned(),
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

#[test]
fn handle_file_size_populates_and_reuses_aggregate_cache() {
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

fn graph_effects_file_size_request(workspace: &Path, request_id: u64) -> FileSizeRequest {
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
            specifier: "array-graph-effects-lib".to_owned(),
            package_name: "array-graph-effects-lib".to_owned(),
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

#[test]
fn revalidate_document_sizes_omits_non_cacheable_results() {
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

    assert!(
        service
            .revalidate_document_sizes(&request, &stale, || true)
            .is_none(),
        "SWR must not push request-specific diagnostics over a good stale value"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
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
        .find(|partial| partial.indexes.as_deref() == Some(&[0, 1]))
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
            partial.indexes.as_deref() == Some(&[tiny_index])
                && partial.states.first().is_some_and(|state| {
                    state.name == "tiny-lib" && state.status == ImportAnalysisStatus::Ready
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
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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
fn service_reports_and_removes_per_project_cache_shards() {
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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
fn remove_all_clears_registry_resolvers_l1_graph_even_when_no_shard_removed() {
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
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

    // Seed the module-graph cache under a unique entry that exists on disk.
    let graph_workspace = temp_workspace();
    let graph_entry = graph_workspace.join("entry.ts");
    fs::write(&graph_entry, "export const value = 1;\n").expect("graph entry should be written");
    let _ = build_module_graph_cached(&graph_entry).expect("module graph should build");
    assert!(
        peek_cached_module_paths(&graph_entry, ImportRuntime::Component).is_some(),
        "module graph should be seeded before the clear"
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
    // Module-graph cache cleared even though no shard was removed (X-21).
    assert!(
        peek_cached_module_paths(&graph_entry, ImportRuntime::Component).is_none(),
        "All must clear the module-graph cache even when no shard was removed"
    );
    // Generation bumped (X-17).
    assert!(
        import_lens_daemon::cache::memory::cache_generation() > before_gen,
        "All must bump the cache generation"
    );

    fs::remove_dir_all(&l1_workspace).expect("l1 workspace should be removed");
    fs::remove_dir_all(&graph_workspace).expect("graph workspace should be removed");
}

#[test]
fn service_cache_miss_preserves_existing_module_graph_cache() {
    let _graph_cache_guard = GRAPH_CACHE_TEST_LOCK
        .lock()
        .expect("graph cache test lock should be available");
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
    // A node_modules change bumps the cache generation in production (the
    // extension sends node_modules_changed); that forces the fingerprint
    // re-verification which detects the changed dependency.
    import_lens_daemon::cache::memory::bump_cache_generation();
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
    // A node_modules change bumps the cache generation in production (the
    // extension sends node_modules_changed); that forces the fingerprint
    // re-verification which detects the changed transitive dependency.
    import_lens_daemon::cache::memory::bump_cache_generation();
    let second = service.handle_batch(package_batch(&workspace, 2, "parent-lib", "value"));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!first.imports[0].cache_hit);
    assert!(!second.imports[0].cache_hit);
    assert_ne!(first.imports[0].raw_bytes, second.imports[0].raw_bytes);
}

#[test]
fn service_does_not_cache_manifest_fallback_results() {
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
    assert!(
        first.imports[0]
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "manifest_fallback"),
        "{first:?}",
    );
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
fn service_does_not_alias_graph_only_side_effect_result_to_namespace_cache() {
    let workspace = temp_workspace();
    write_array_graph_effects_package(&workspace);
    let service = ImportLensService::new(None, false);

    let named = service.handle_batch(graph_effects_batch(&workspace, 1, ImportKind::Named));
    let namespace = service.handle_batch(graph_effects_batch(&workspace, 2, ImportKind::Namespace));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert!(!named.imports[0].cache_hit);
    assert!(named.imports[0].side_effects, "{named:?}");
    assert!(!namespace.imports[0].cache_hit);
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
fn service_file_size_includes_graph_only_side_effect_modules() {
    let workspace = temp_workspace();
    write_array_graph_effects_package(&workspace);
    let service = ImportLensService::new(None, false);

    let response = service.handle_file_size(graph_effects_file_size_request(&workspace, 26));

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
    assert_eq!(response.error, None);
    assert_eq!(response.imports.len(), 1);
    assert_eq!(response.raw_bytes, response.imports[0].raw_bytes);
    assert!(response.diagnostics.iter().any(|diagnostic| {
        diagnostic.stage == "side_effects"
            && diagnostic.details.iter().any(|detail| {
                detail
                    .replace('\\', "/")
                    .contains("dist/polyfill/browser/setup.js")
            })
    }));
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
