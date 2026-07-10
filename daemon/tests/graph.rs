use import_lens_daemon::ipc::protocol::ImportRuntime;
use import_lens_daemon::pipeline::{
    graph::{
        GraphLimits, MAX_CACHED_GRAPHS, ModuleGraph, build_module_graph, build_module_graph_cached,
        build_module_graph_cached_with_runtime, build_module_graph_with_limits,
        cached_module_graph_with_runtime, clear_module_graph_cache, module_graph_cache_len,
        peek_cached_module_paths, purge_missing_module_graphs,
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

/// Lowering an enum needs each member's evaluated value, which only lands in `Scoping`
/// when the semantic builder is configured with `with_enum_eval(true)`.
///
/// Assert on the emitted code, not just on "it did not panic". The transformer's guard
/// is a `debug_assert!`, so it is compiled out of the release daemon we ship: without
/// the flag a release build does not crash, it silently emits
/// `Level["High"] = 1 + Level["Low"]` where it should emit `Level["High"] = 1`. That is
/// a larger bundle for the same program, which is exactly the thing this tool measures.
/// `High` is left implicit on purpose — an explicit `= 1` is folded either way and
/// would hide the bug.
#[test]
fn typescript_enum_member_values_are_folded_not_recomputed() {
    let (root, _entry_path, graph) = graph_from_sources([
        ("entry.js", "export { label } from \"./level.ts\";\n"),
        (
            "level.ts",
            "export enum Level {\n  Low = 0,\n  High,\n}\nexport const label = Level[Level.High];\n",
        ),
    ]);

    let module = graph
        .modules
        .iter()
        .find(|module| module.path.ends_with("level.ts"))
        .map(|module| module.source.clone());

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");

    let source = module.expect("the TypeScript module should be resolved into the graph");
    assert!(
        source.contains("Level[\"High\"] = 1]") || source.contains("Level[\"High\"] = 1;"),
        "implicit enum member should be folded to its constant value: {source}"
    );
    assert!(
        !source.contains("+ Level[\"Low\"]"),
        "enum member value was recomputed at runtime instead of folded: {source}"
    );
}

#[test]
fn binding_dependencies_track_top_level_references() {
    let (root, _entry_path, graph) = graph_from_sources([(
        "entry.js",
        "const helper = 1;\nconst value = helper + 1;\nexport const total = value + helper;\n",
    )]);

    let entry = graph
        .module_by_id(graph.entry_id)
        .expect("entry module should exist");
    let mut deps: Vec<(String, String)> = entry
        .binding_dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.binding_name.clone(),
                dependency.referenced_name.clone(),
            )
        })
        .collect();
    deps.sort();

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    assert_eq!(
        deps,
        vec![
            ("total".to_owned(), "helper".to_owned()),
            ("total".to_owned(), "value".to_owned()),
            ("value".to_owned(), "helper".to_owned()),
        ]
    );
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
fn purge_missing_module_graphs_drops_graphs_for_deleted_entries() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().expect("graph cache test lock");
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const value = 1;");
    clear_module_graph_cache();

    build_module_graph_cached(&root.join("entry.js")).expect("graph should build");
    assert_eq!(module_graph_cache_len(), 1);

    // Uninstall the package: the cached graph's entry path no longer exists.
    fs::remove_dir_all(&root).expect("temp graph workspace should be removed");
    let removed = purge_missing_module_graphs();

    clear_module_graph_cache();
    assert_eq!(removed, 1);
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

// RB-1 / X-7: an equal-length, mtime-preserving rewrite of a first-party module
// (cp -p, rsync -a, tar -x, some formatters) leaves len+mtime identical, so the
// old non-strict pre-filter reused the STALE cached graph — and because L2
// recomputes THROUGH this cache, the stale size was served forever. The strict
// gate hash-verifies first-party modules and must rebuild.
#[test]
fn graph_cache_rebuilds_on_mtime_preserving_first_party_edit() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().expect("graph cache test lock");
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { value } from './dep.js';\nexport const answer = value;",
    );
    let dep = root.join("dep.js");
    // Two bodies of IDENTICAL length, differing only in content.
    fs::write(&dep, "export const value = 'aaaaa';").expect("write dep");
    clear_module_graph_cache();

    let original_mtime = fs::metadata(&dep)
        .and_then(|meta| meta.modified())
        .expect("dep mtime");

    let first = build_module_graph_cached(&root.join("entry.js")).expect("graph should build");

    // Same-length rewrite, then restore the ORIGINAL mtime so len+mtime match the
    // cached fingerprint exactly — only the content hash differs.
    fs::write(&dep, "export const value = 'bbbbb';").expect("rewrite dep");
    fs::File::options()
        .write(true)
        .open(&dep)
        .and_then(|file| file.set_modified(original_mtime))
        .expect("restore dep mtime");
    assert_eq!(
        fs::metadata(&dep).and_then(|meta| meta.modified()).ok(),
        Some(original_mtime),
        "test setup must preserve mtime so a non-strict gate would have served stale",
    );

    let second = build_module_graph_cached(&root.join("entry.js"))
        .expect("graph should rebuild after a mtime-preserving content change");

    fs::remove_dir_all(root).expect("temp graph workspace should be removed");
    clear_module_graph_cache();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the strict gate must rebuild on a mtime-preserving first-party edit",
    );
    assert!(
        second
            .modules
            .iter()
            .any(|module| module.source.contains("bbbbb")),
        "the rebuilt graph must reflect the new content: {second:?}",
    );
}

#[test]
fn peek_cached_module_paths_returns_module_set_without_freshness_gate() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().expect("graph cache test lock");
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { value } from './dep.js';\nexport const answer = value;\n",
    );
    write_source(&root, "dep.js", "export const value = 'before';\n");
    clear_module_graph_cache();

    let entry = fs::canonicalize(root.join("entry.js")).expect("canonical entry");
    let dep = fs::canonicalize(root.join("dep.js")).expect("canonical dep");

    build_module_graph_cached_with_runtime(&entry, ImportRuntime::Component)
        .expect("graph should build");

    // Fresh: the gated accessor hits, and the peek returns the full module set —
    // including the deep dependency L1 must be able to re-stat.
    assert!(cached_module_graph_with_runtime(&entry, ImportRuntime::Component).is_some());
    let peeked_fresh =
        peek_cached_module_paths(&entry, ImportRuntime::Component).expect("peek while fresh");
    assert!(
        peeked_fresh.iter().any(|path| path == &entry),
        "peek must include the entry module"
    );
    assert!(
        peeked_fresh.iter().any(|path| path == &dep),
        "peek must include the deep dependency module"
    );

    // Edit the deep dependency so the cached graph's fingerprints go stale.
    thread::sleep(Duration::from_millis(2));
    write_source(&root, "dep.js", "export const value = 'after the edit';\n");

    // The freshness-gated accessor now reports None (stale) without evicting the entry,
    // but the L1 peek must STILL return the same raw path set — a gate here would return
    // None exactly when a module changed, blinding L1 to the edit it must catch.
    assert!(
        cached_module_graph_with_runtime(&entry, ImportRuntime::Component).is_none(),
        "gated accessor must treat the edited graph as stale"
    );
    let peeked_stale = peek_cached_module_paths(&entry, ImportRuntime::Component)
        .expect("peek must ignore the freshness gate");
    assert_eq!(
        peeked_fresh, peeked_stale,
        "peek must return the same module set regardless of freshness"
    );

    fs::remove_dir_all(&root).ok();
    clear_module_graph_cache();
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

#[test]
fn module_record_carries_root_symbol_and_shorthand_spans() {
    let (root, _entry, graph) = graph_from_sources([(
        "entry.js",
        "const helper = 1;\nconst obj = { helper };\nexport const value = helper + obj.helper;",
    )]);

    let entry = graph
        .modules
        .iter()
        .find(|module| module.path.ends_with("entry.js"))
        .expect("entry module");

    let helper = entry
        .root_symbol_spans
        .iter()
        .find(|symbol| symbol.name == "helper")
        .expect("helper symbol spans");
    assert!(helper.decl.0 < helper.decl.1);
    assert!(!helper.references.is_empty());

    assert!(
        !entry.shorthand_spans.is_empty(),
        "shorthand object property should be recorded"
    );

    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn graph_transforms_plain_jsx_shipped_in_js_modules() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { Widget } from './widget.js';\nexport const value = Widget;",
    );
    write_source(
        &root,
        "widget.js",
        "export const Widget = () => <div className=\"x\">hi</div>;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("plain JSX in .js should build");

    let widget = graph
        .modules
        .iter()
        .find(|module| module.path.ends_with("widget.js"))
        .expect("widget module");
    let widget_source = widget.source.clone();
    fs::remove_dir_all(root).expect("cleanup");
    assert!(!widget_source.contains("<div"), "{widget_source}");
}

#[test]
fn graph_still_fails_gracefully_on_flow_typed_js() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { x } from './flow.js';\nexport const value = x;",
    );
    write_source(&root, "flow.js", "export const x: number = 1;");

    let result = build_module_graph(&root.join("entry.js"));

    fs::remove_dir_all(root).expect("cleanup");
    assert!(result.is_err(), "Flow-typed .js should fail, not panic");
}
