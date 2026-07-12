//! Export enumeration is memoized per (entry, runtime).
//!
//! Completion asks a package "what do you export?" and the daemon answered with a
//! full, uncached Rolldown build of the entire package graph — on every popup, for a
//! list that only changes when the package's files do.
//!
//! Measured through the engine's own build counter, which is the honest unit. Both the
//! counter and the memo are process-global, so this test owns its binary.

use import_lens_daemon::engine::boundary::builds_started;
use import_lens_daemon::ipc::protocol::ImportRuntime;
use import_lens_daemon::pipeline::export_list::enumerate_exports_cached;
use std::{fs, path::Path, path::PathBuf};

mod common;

fn write_package(workspace: &Path) -> PathBuf {
    let root = workspace.join("node_modules").join("pkg");
    fs::create_dir_all(&root).expect("package root should be created");
    fs::write(
        root.join("index.js"),
        "export { alpha } from './alpha.js';\nexport const beta = 2;\n",
    )
    .expect("entry should be written");
    write_alpha(workspace, "alpha");
    root.join("index.js")
}

fn write_alpha(workspace: &Path, body: &str) {
    let path = workspace.join("node_modules").join("pkg").join("alpha.js");
    fs::write(path, format!("export const alpha = () => '{body}';\n")).expect("alpha");
}

fn enumerate(entry: &Path) -> (Vec<String>, usize) {
    let before = builds_started();
    let enumeration = enumerate_exports_cached(entry, ImportRuntime::Component)
        .expect("enumeration should succeed");
    let mut names = enumeration.names;
    names.sort();
    (names, builds_started() - before)
}

#[test]
fn enumeration_is_built_once_and_expires_when_a_module_it_read_changes() {
    let workspace = common::temp_workspace("import-lens-export-memo");
    let entry = write_package(&workspace);

    let (names, builds) = enumerate(&entry);
    assert_eq!(names, vec!["alpha".to_owned(), "beta".to_owned()]);
    assert_eq!(builds, 1, "a cold enumeration builds once");

    let (names, builds) = enumerate(&entry);
    assert_eq!(names, vec!["alpha".to_owned(), "beta".to_owned()]);
    assert_eq!(
        builds, 0,
        "a repeat completion popup must not rebuild the package graph"
    );

    // A module the enumeration read changed. Same length, fresh mtime — the memo
    // validates with the same strict, hash-verifying check the import cache uses.
    write_alpha(&workspace, "gamma");
    let (_, builds) = enumerate(&entry);
    assert_eq!(
        builds, 1,
        "editing a module the enumeration read must expire it"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}
