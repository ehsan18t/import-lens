//! Temporary review repros — verifies suspected bugs against CURRENT behavior.
//! Each test asserts what the code does TODAY so a failure means the suspicion
//! was wrong. This file is deleted once findings are confirmed.

use import_lens_daemon::{
    document::is_runtime_package_specifier,
    pipeline::{
        bundle::bundle_reachable_modules_with_metadata,
        graph::build_module_graph,
        reachability::reachable_exports,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-review")
}

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

/// Control: the same chain with a DIRECT export in the star target works.
#[test]
fn control_star_direct_export_is_reached() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './c.js';");
    write_source(
        &root,
        "c.js",
        "export const x = 1;\nexport const y = 'HEAVY_UNUSED_PAYLOAD';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should build");
    let reachable = reachable_exports(&graph, &["x".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("bundle should not error");

    fs::remove_dir_all(&root).expect("cleanup");
    assert!(bundled.source.contains("__il_m1_x"), "{}", bundled.source);
    assert!(
        !bundled.source.contains("HEAVY_UNUSED_PAYLOAD"),
        "{}",
        bundled.source
    );
}

/// SUSPICION 2: `resolve_export_binding` recurses without a visited set, so a
/// star-export cycle plus an unresolvable imported name overflows the stack.
/// Ignored by default: it ABORTS the whole test process when it reproduces.
/// Run explicitly: cargo test --test review_repros -- --ignored star_cycle
#[test]
#[ignore = "aborts the process (stack overflow) when the bug reproduces"]
fn repro_star_cycle_stack_overflow() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { x } from './a.js';\nexport const value = x;",
    );
    write_source(&root, "a.js", "export * from './b.js';");
    write_source(&root, "b.js", "export * from './a.js';");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should build");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    // Expected today: this call never returns (stack overflow → abort).
    let _ = bundle_reachable_modules_with_metadata(&graph, &reachable);

    fs::remove_dir_all(&root).expect("cleanup");
}

/// SUSPICION 4: bare Node builtin subpaths are treated as npm packages.
#[test]
fn repro_builtin_subpath_treated_as_package() {
    // CURRENT (buggy) behavior: true (analyzed as npm package "fs").
    assert!(is_runtime_package_specifier("fs/promises"));
    // Controls that already work:
    assert!(!is_runtime_package_specifier("fs"));
    assert!(!is_runtime_package_specifier("node:fs/promises"));
}

/// SUSPICION 6: JSON with `eval`/`arguments` keys synthesizes an invalid
/// module (`export const eval = ...` is a strict-mode SyntaxError).
#[test]
fn repro_json_eval_key_breaks_graph() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import data from './data.json';\nexport const value = data;",
    );
    write_source(&root, "data.json", "{\"eval\": 1, \"safe\": 2}");

    let result = build_module_graph(&root.join("entry.js"));

    fs::remove_dir_all(&root).expect("cleanup");
    // CURRENT (buggy) behavior: graph build fails on the synthetic module.
    assert!(result.is_err(), "graph built: {result:?}");
}
