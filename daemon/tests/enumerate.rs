//! Export enumeration must answer under the import's runtime, and its memo must expire
//! when the package manifest changes.
//!
//! Both facts are process-global (the shared export-list memo and the engine's build
//! counter), so every test here serializes on one lock and this file owns its binary.

use import_lens_daemon::{
    engine::boundary::builds_started,
    ipc::protocol::{CompleteImportMembersRequest, ImportRuntime, PROTOCOL_VERSION},
    pipeline::analyze::AnalysisContext,
    pipeline::export_list::enumerate_exports_cached,
    service::ImportLensService,
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

mod common;

static ENUMERATE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENUMERATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A package whose `.` export resolves to a DIFFERENT file under browser conditions
/// (Component/Client) than under node conditions (Server), each exporting a different
/// name. Enumerating it reveals which runtime the daemon resolved under.
fn write_dual_runtime_package(workspace: &Path) {
    let root = workspace.join("node_modules").join("dual-pkg");
    std::fs::create_dir_all(&root).expect("package root");
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"dual-pkg","version":"1.0.0","exports":{".":{"browser":"./browser.js","node":"./node.js","default":"./node.js"}}}"#,
    )
    .expect("manifest");
    std::fs::write(root.join("browser.js"), "export const browserOnly = 1;\n").expect("browser");
    std::fs::write(root.join("node.js"), "export const nodeOnly = 1;\n").expect("node");
}

const ASTRO_SERVER_IMPORT: &str = "---\nimport {  } from 'dual-pkg';\n---\n<h1>hi</h1>\n";

/// Task 10. Astro frontmatter is a Server region: its imports resolve under node
/// conditions. A completion there must offer the NODE export surface, not the browser
/// one — otherwise completions offer names the file cannot import and hide names it can,
/// while the SIZE of the same import is computed under Server (they disagree by
/// construction). Before the fix the enumeration entry point hardcoded Component.
#[test]
fn completion_in_astro_frontmatter_enumerates_the_server_surface() {
    let _guard = lock();
    let workspace = common::temp_workspace("import-lens-enumerate-runtime");
    write_dual_runtime_package(&workspace);
    let document = workspace.join("src").join("pages").join("index.astro");
    std::fs::create_dir_all(document.parent().expect("document parent")).expect("document dir");
    std::fs::write(&document, ASTRO_SERVER_IMPORT).expect("astro document");

    let cursor_offset = ASTRO_SERVER_IMPORT.find('{').expect("import brace") + 1;

    let service = ImportLensService::new(None, false);
    let response = service.complete_import_members(CompleteImportMembersRequest {
        message_type: "complete_import_members".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 1,
        workspace_root: workspace.to_string_lossy().into_owned(),
        active_document_path: document.to_string_lossy().into_owned(),
        source: ASTRO_SERVER_IMPORT.to_owned(),
        cursor_offset,
    });

    std::fs::remove_dir_all(&workspace).ok();

    assert_eq!(response.error, None, "{response:?}");
    assert_eq!(response.specifier.as_deref(), Some("dual-pkg"));
    assert!(
        response.exports.contains(&"nodeOnly".to_owned()),
        "a Server-region completion must return the node surface, got {:?}",
        response.exports
    );
    assert!(
        !response.exports.contains(&"browserOnly".to_owned()),
        "a Server-region completion must not offer the browser surface, got {:?}",
        response.exports
    );
}

fn enumerate(workspace: &Path, package_root: &Path, entry: &Path) -> (Vec<String>, usize) {
    let context = AnalysisContext {
        workspace_root: workspace.to_path_buf(),
        active_document_path: workspace.join("src").join("app.ts"),
    };
    let before = builds_started();
    let enumeration =
        enumerate_exports_cached(&context, package_root, entry, ImportRuntime::Component)
            .expect("enumeration should build");
    let mut names = enumeration.names;
    names.sort();
    (names, builds_started() - before)
}

/// Task 11. The export-list memo stored only the source-module fingerprints and never the
/// package manifest, so editing a first-party package's `package.json` (its `type`,
/// `exports`, or `sideEffects`) moved no fingerprint and the stale export list was served
/// forever. The size path already fingerprints the manifests (§8.3); enumeration did not.
#[test]
fn enumeration_memo_expires_when_the_package_manifest_changes() {
    let _guard = lock();
    let workspace = common::temp_workspace("import-lens-enumerate-manifest");
    let package_root: PathBuf = workspace.join("lib");
    std::fs::create_dir_all(&package_root).expect("package dir");
    let manifest = package_root.join("package.json");
    std::fs::write(
        &manifest,
        r#"{"name":"lib","version":"1.0.0","type":"module","sideEffects":false}"#,
    )
    .expect("manifest");
    let entry = package_root.join("index.js");
    std::fs::write(&entry, "export const alpha = 1;\nexport const beta = 2;\n").expect("entry");

    let (names, builds) = enumerate(&workspace, &package_root, &entry);
    assert_eq!(names, vec!["alpha".to_owned(), "beta".to_owned()]);
    assert_eq!(builds, 1, "a cold enumeration builds once");

    let (_, builds) = enumerate(&workspace, &package_root, &entry);
    assert_eq!(
        builds, 0,
        "a repeat completion popup must not rebuild the graph"
    );

    // Edit ONLY the manifest — no source file moves, no cache generation bump.
    std::fs::write(
        &manifest,
        r#"{"name":"lib","version":"1.0.0","type":"module","sideEffects":true}"#,
    )
    .expect("manifest edit");

    let (_, builds) = enumerate(&workspace, &package_root, &entry);
    assert_eq!(
        builds, 1,
        "editing the package manifest must expire the memo"
    );

    std::fs::remove_dir_all(&workspace).ok();
}
