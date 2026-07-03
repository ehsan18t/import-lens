use import_lens_daemon::pipeline::{
    graph::{
        GraphLimits, MAX_CACHED_GRAPHS, ModuleGraph, build_module_graph, build_module_graph_cached,
        build_module_graph_with_limits, clear_module_graph_cache, module_graph_cache_len,
    },
    reachability::reachable_exports,
};
use std::sync::{Arc, Mutex};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

mod common;

static GRAPH_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn temp_workspace() -> PathBuf {
    let path = common::temp_workspace("import-lens-graph");
    fs::canonicalize(&path).expect("temp graph workspace should be canonicalized")
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
    let lib_path = root.join("lib.js");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(reachable.contains_module_symbol(&lib_path, "used"));
    assert!(!reachable.contains_module_symbol(&lib_path, "unused"));
}

#[test]
fn graph_marks_all_entry_exports_reachable_for_full_module_imports() {
    let (root, _entry_path, graph) = graph_from_sources([(
        "entry.js",
        "export const used = 1;\nexport const unused = 2;",
    )]);

    let reachable = reachable_exports(&graph, &[], true);
    let entry_path = root.join("entry.js");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(reachable.contains_module_symbol(&entry_path, "used"));
    assert!(reachable.contains_module_symbol(&entry_path, "unused"));
}

#[test]
fn graph_keeps_side_effect_only_imports_reachable() {
    let (root, _entry_path, graph) = graph_from_sources([
        ("entry.js", "import './setup.js';\nexport const value = 1;"),
        ("setup.js", "globalThis.__importLensSetup = true;"),
    ]);
    let setup_path = root.join("setup.js");
    let entry_path = root.join("entry.js");

    let reachable = reachable_exports(&graph, &["value".to_owned()], false);

    assert!(reachable.contains_module_symbol(&entry_path, "value"));
    assert!(reachable.contains_module(&setup_path));
    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
}

#[test]
fn graph_rejects_module_count_above_limit() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import './one.js';\nimport './two.js';\nexport const value = 1;",
    );
    write_source(&root, "one.js", "export const one = 1;");
    write_source(&root, "two.js", "export const two = 2;");

    let error = build_module_graph_with_limits(
        &root.join("entry.js"),
        GraphLimits {
            max_modules: 2,
            max_module_source_bytes: 1024,
            max_graph_source_bytes: 4096,
        },
    )
    .expect_err("third module should exceed the limit");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(error.contains("module count"));
}

#[test]
fn graph_rejects_single_module_above_source_limit() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const value = 'too long';");

    let error = build_module_graph_with_limits(
        &root.join("entry.js"),
        GraphLimits {
            max_modules: 10,
            max_module_source_bytes: 8,
            max_graph_source_bytes: 4096,
        },
    )
    .expect_err("entry module should exceed the source limit");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(error.contains("module source"));
}

#[test]
fn graph_rejects_total_source_above_limit() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import './one.js';\nexport const value = 1;",
    );
    write_source(&root, "one.js", "export const one = 'large enough';");

    let error = build_module_graph_with_limits(
        &root.join("entry.js"),
        GraphLimits {
            max_modules: 10,
            max_module_source_bytes: 1024,
            max_graph_source_bytes: 48,
        },
    )
    .expect_err("combined sources should exceed the graph source limit");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(error.contains("graph source"));
}

#[test]
fn graph_resolves_bare_transitive_dependency_modules() {
    let root = temp_workspace();
    write_source(
        &root,
        "packages/main/index.js",
        "import { helper } from 'dep-lib';\nexport const value = helper();",
    );
    write_source(
        &root,
        "node_modules/dep-lib/package.json",
        r#"{"version":"1.0.0","module":"index.js"}"#,
    );
    write_source(
        &root,
        "node_modules/dep-lib/index.js",
        "export const helper = () => 42;",
    );

    let graph = build_module_graph(&root.join("packages/main/index.js"))
        .expect("graph should resolve bare dependency");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(
        graph
            .modules
            .iter()
            .any(|module| module.path.to_string_lossy().contains("dep-lib")),
        "{graph:?}",
    );
    assert!(
        graph
            .dependency_paths
            .iter()
            .any(|path| path.to_string_lossy().contains("dep-lib")),
        "{:?}",
        graph.dependency_paths
    );
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
}

#[test]
fn graph_resolves_relative_directory_package_manifest() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import createClient from './createClient';\nexport const value = createClient();",
    );
    write_source(
        &root,
        "createClient/package.json",
        r#"{"name":"create-client-fixture","browser":"./browser.js","main":"./node.js"}"#,
    );
    write_source(
        &root,
        "createClient/browser.js",
        "export default function createClient() { return 'browser'; }",
    );
    write_source(
        &root,
        "createClient/node.js",
        "export default function createClient() { return 'node'; }",
    );

    let graph = build_module_graph(&root.join("entry.js"))
        .expect("relative directory package manifest should resolve");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(
        graph
            .modules
            .iter()
            .any(|module| module.path.ends_with("createClient/browser.js")),
        "{graph:?}",
    );
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
}

#[test]
fn graph_keeps_builtins_and_unresolved_peers_external_with_diagnostics() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import fs from 'node:fs';\nimport peer from 'missing-peer';\nexport const value = fs && peer;",
    );

    let graph = build_module_graph(&root.join("entry.js"))
        .expect("graph should keep externals instead of failing");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    let entry = graph
        .module_by_id(graph.entry_id)
        .expect("entry module should exist");
    assert_eq!(entry.external_imports.len(), 2);
    assert!(
        graph
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("node:fs")),
        "{:?}",
        graph.diagnostics
    );
    assert!(
        graph
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing-peer")),
        "{:?}",
        graph.diagnostics
    );
}

#[test]
fn graph_does_not_report_shared_dependency_as_cycle() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { one } from './one.js';\nimport { two } from './two.js';\nexport const value = one + two;",
    );
    write_source(
        &root,
        "one.js",
        "import { shared } from './shared.js';\nexport const one = shared;",
    );
    write_source(
        &root,
        "two.js",
        "import { shared } from './shared.js';\nexport const two = shared;",
    );
    write_source(&root, "shared.js", "export const shared = 1;");

    let graph =
        build_module_graph(&root.join("entry.js")).expect("graph should allow shared dependencies");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert!(
        !graph
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "circular_dependency"),
        "{:?}",
        graph.diagnostics
    );
}

#[test]
fn graph_cache_returns_shared_graph_handle_without_deep_clone() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().expect("graph cache test lock");
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const value = 1;");
    clear_module_graph_cache();

    let first = build_module_graph_cached(&root.join("entry.js")).expect("graph should build");
    let second = build_module_graph_cached(&root.join("entry.js")).expect("graph should be cached");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    clear_module_graph_cache();
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn graph_cache_rebuilds_when_dependency_file_changes() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().expect("graph cache test lock");
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { value } from './dep.js';\nexport const answer = value;",
    );
    write_source(&root, "dep.js", "export const value = 'before';");
    clear_module_graph_cache();

    let first = build_module_graph_cached(&root.join("entry.js")).expect("graph should build");
    thread::sleep(Duration::from_millis(2));
    write_source(
        &root,
        "dep.js",
        "export const value = 'after dependency change';",
    );
    let second = build_module_graph_cached(&root.join("entry.js"))
        .expect("graph should rebuild after dependency changes");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    clear_module_graph_cache();
    assert!(!Arc::ptr_eq(&first, &second));
    assert!(
        second
            .modules
            .iter()
            .any(|module| module.source.contains("after dependency change")),
        "{second:?}",
    );
}

static NEXT_TEMP_GRAPH_WORKSPACE_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn temp_graph_workspace() -> PathBuf {
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let id = NEXT_TEMP_GRAPH_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    let process_id = std::process::id();
    let path = std::env::temp_dir().join(format!("import-lens-graph-{process_id}-{suffix}-{id}"));
    fs::create_dir_all(&path).expect("temp graph workspace should be created");
    path
}

#[test]
fn graph_resolves_and_transforms_mts_and_cts_modules() {
    let workspace = temp_graph_workspace();

    for extension in ["mts", "cts"] {
        let entry = workspace.join(format!("entry.{extension}"));
        let dep = workspace.join(format!("dep.{extension}"));
        fs::write(
            &entry,
            "import { value } from './dep';\nexport const answer: number = value;\n",
        )
        .expect("entry module should be written");
        fs::write(&dep, "export const value: number = 42;\n")
            .expect("dep module should be written");

        let graph = build_module_graph(&entry).expect("graph should build");

        assert_eq!(graph.modules.len(), 2);
        assert!(
            graph
                .modules
                .iter()
                .all(|module| !module.source.contains(": number")),
            "{graph:?}",
        );
    }

    fs::remove_dir_all(workspace).expect("temp graph workspace should be removed");
}

#[test]
fn json_modules_with_strict_mode_restricted_keys_still_build() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import data from './data.json';\nexport const value = data;",
    );
    write_source(
        &root,
        "data.json",
        "{\"eval\": 1, \"arguments\": 2, \"safe\": 3}",
    );

    let graph = build_module_graph(&root.join("entry.js"));

    fs::remove_dir_all(root).expect("temp workspace should be removed");
    let graph = graph.expect("JSON with eval/arguments keys should build");
    let json_module = graph
        .modules
        .iter()
        .find(|module| module.path.extension().is_some_and(|ext| ext == "json"))
        .expect("json module should be in the graph");
    assert!(json_module.source.contains("export const safe"));
    assert!(!json_module.source.contains("export const eval"));
    assert!(!json_module.source.contains("export const arguments"));
}

#[test]
fn graph_cache_evicts_least_recently_used_beyond_cap() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().expect("graph cache test lock");
    clear_module_graph_cache();
    let root = temp_workspace();

    for index in 0..(MAX_CACHED_GRAPHS + 3) {
        let entry = format!("entry{index}.js");
        write_source(
            &root,
            &entry,
            &format!("export const value{index} = {index};"),
        );
        build_module_graph_cached(&root.join(&entry)).expect("graph should build");
    }

    fs::remove_dir_all(root).expect("temp workspace should be removed");
    assert!(module_graph_cache_len() <= MAX_CACHED_GRAPHS);
    clear_module_graph_cache();
}
