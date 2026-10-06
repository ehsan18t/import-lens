//! A file whose combined build fails deterministically does not rebuild it on every size request.
//!
//! Its total is a floor and is never cached, which is right; re-running the file's biggest build
//! only to reproduce the same failure is not. The failure is memoized against the bytes it was
//! derived from and the cache generation, so fixing the broken module, or an invalidation, builds
//! again.
//!
//! Measured through the engine's own build counter. The counter and the memo are process-global,
//! so this test owns its binary.

use import_lens_daemon::cache::memory::bump_cache_generation;
use import_lens_daemon::engine::boundary::builds_started;
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::analyze::AnalysisContext;
use import_lens_daemon::pipeline::file_size::{
    FileSizeComputation, SizedImport, compute_file_size,
};
use std::{fs, path::Path};

mod common;

fn write_package(workspace: &Path, name: &str, source: &str) {
    let root = workspace.join("node_modules").join(name);
    fs::create_dir_all(&root).expect("package root");
    fs::write(
        root.join("package.json"),
        format!(r#"{{"name":"{name}","version":"1.0.0","type":"module","module":"./index.js"}}"#),
    )
    .expect("manifest");
    fs::write(root.join("index.js"), source).expect("entry");
}

fn import(name: &str) -> SizedImport {
    SizedImport::installed(
        ImportRequest {
            specifier: name.to_owned(),
            package_name: name.to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["value".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        },
        None,
    )
}

fn size(workspace: &Path) -> (FileSizeComputation, usize) {
    let context = AnalysisContext {
        workspace_root: workspace.to_path_buf(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let before = builds_started();
    let computed = compute_file_size(&context, &[import("good"), import("broken")]);
    (computed, builds_started() - before)
}

fn stages(computed: &FileSizeComputation) -> Vec<String> {
    computed
        .diagnostics
        .iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.stage, diagnostic.message))
        .collect()
}

const BROKEN: &str = "export const value = ;\n";
const FIXED: &str = "export const value = 2;\n";

#[test]
fn a_deterministic_combined_build_failure_is_built_once_until_its_bytes_change() {
    let workspace = common::temp_workspace("import-lens-file-size-failure-memo");
    write_package(&workspace, "good", "export const value = 1;\n");
    write_package(&workspace, "broken", BROKEN);

    let (first, builds) = size(&workspace);
    assert_eq!(builds, 1, "a cold request builds once");
    assert!(first.degraded, "the combined build failed");
    assert!(
        first.diagnostics.iter().any(|item| item.stage == "parse"),
        "the failure is a parse failure: {:?}",
        stages(&first)
    );

    let (second, builds) = size(&workspace);
    assert_eq!(
        builds, 0,
        "a repeat request must not rebuild a combined build that failed deterministically"
    );
    assert_eq!(
        stages(&second),
        stages(&first),
        "the memo answers the same failure"
    );
    assert_eq!(
        (second.degraded, second.incomplete),
        (first.degraded, first.incomplete)
    );

    write_package(&workspace, "broken", FIXED);
    let (fixed, builds) = size(&workspace);
    assert_eq!(builds, 1, "fixing the broken module must expire the memo");
    assert!(
        !fixed.degraded,
        "the fixed file builds: {:?}",
        stages(&fixed)
    );

    write_package(&workspace, "broken", BROKEN);
    let (_, builds) = size(&workspace);
    assert_eq!(builds, 1, "breaking it again builds once");
    let (_, builds) = size(&workspace);
    assert_eq!(builds, 0);
    bump_cache_generation();
    let (_, builds) = size(&workspace);
    assert_eq!(builds, 1, "a cache invalidation must expire the memo");

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}
