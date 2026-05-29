use import_lens_daemon::pipeline::{
    graph::{ModuleGraph, build_module_graph},
    reachability::reachable_exports,
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
    let path = std::env::temp_dir().join(format!("import-lens-graph-{suffix}"));
    fs::create_dir_all(&path).expect("temp graph workspace should be created");
    path
}

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn graph_from_sources<const N: usize>(
    sources: [(&str, &str); N],
) -> (PathBuf, PathBuf, ModuleGraph) {
    let root = temp_workspace();
    for (relative_path, source) in sources {
        write_source(&root, relative_path, source);
    }
    let entry_path = root.join("entry.js");
    let graph = build_module_graph(&entry_path).expect("module graph should be built");

    (root, entry_path, graph)
}

#[test]
fn graph_marks_only_requested_named_export_reachable() {
    let (root, _entry_path, graph) = graph_from_sources([
        ("entry.js", "export { used } from './lib.js';"),
        (
            "lib.js",
            "export const used = 1;\nexport const unused = heavy();",
        ),
    ]);

    let reachable = reachable_exports(&graph, &["used".to_owned()], false);

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(reachable.contains_symbol("used"));
    assert!(!reachable.contains_symbol("unused"));
}

#[test]
fn graph_marks_all_entry_exports_reachable_for_full_module_imports() {
    let (root, _entry_path, graph) = graph_from_sources([(
        "entry.js",
        "export const used = 1;\nexport const unused = 2;",
    )]);

    let reachable = reachable_exports(&graph, &[], true);

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(reachable.contains_symbol("used"));
    assert!(reachable.contains_symbol("unused"));
}

#[test]
fn graph_keeps_side_effect_only_imports_reachable() {
    let (root, _entry_path, graph) = graph_from_sources([
        ("entry.js", "import './setup.js';\nexport const value = 1;"),
        ("setup.js", "globalThis.__importLensSetup = true;"),
    ]);
    let setup_path = root.join("setup.js");

    let reachable = reachable_exports(&graph, &["value".to_owned()], false);

    assert!(reachable.contains_symbol("value"));
    assert!(reachable.contains_module(&setup_path));
    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
}
