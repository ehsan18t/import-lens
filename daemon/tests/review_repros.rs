//! Temporary review repros — verifies suspected bugs against CURRENT behavior.
//! Each test asserts what the code does TODAY so a failure means the suspicion
//! was wrong. This file is deleted once findings are confirmed.

use import_lens_daemon::{
    document::{is_runtime_package_specifier, named_import_completion_context},
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

/// SUSPICION 3: completion bails when any earlier import statement lacks
/// braces (e.g. a default import on line 1).
#[test]
fn repro_completion_bails_on_earlier_braceless_import() {
    let source = "import React from 'react';\nimport { map } from 'lodash';\n";
    let cursor = source.rfind('{').expect("brace exists") + 1;

    let context = named_import_completion_context(source, cursor);

    // CURRENT (buggy) behavior: None even though the cursor is inside braces.
    assert!(context.is_none(), "got: {context:?}");

    // Control: without the braceless import first, completion works.
    let control_source = "import { map } from 'lodash';\n";
    let control_cursor = control_source.rfind('{').expect("brace exists") + 1;
    let control = named_import_completion_context(control_source, control_cursor);
    assert!(control.is_some());
}

/// SUSPICION 3b (extension cross-check): the extension sends
/// `document.offsetAt(position)` — a UTF-16 code-unit offset — but the daemon
/// compares `cursor_offset` against byte offsets from oxc spans. Any
/// non-ASCII character before the cursor desynchronizes the two.
#[test]
fn repro_completion_cursor_offset_is_bytes_but_client_sends_utf16() {
    let source = "const s = '\u{20AC}\u{20AC}';\nimport { map } from 'lodash';\n";
    let byte_cursor = source.rfind('{').expect("brace exists") + 1;
    let utf16_cursor: usize = source[..byte_cursor].chars().map(char::len_utf16).sum();

    // Two euro signs: 6 bytes but 2 UTF-16 units, so the offsets diverge.
    assert_eq!(byte_cursor, utf16_cursor + 4);

    // CURRENT (buggy) behavior: the client's UTF-16 offset misses the braces.
    assert!(named_import_completion_context(source, utf16_cursor).is_none());
    // Control: the byte offset the daemon actually expects works.
    assert!(named_import_completion_context(source, byte_cursor).is_some());
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

/// SUSPICION 7 (oxc source-type cross-check): `.js` documents parse without
/// the JSX variant, so CRA-style JSX in `.js` files fails import analysis.
#[test]
fn repro_jsx_in_js_documents_fails_analysis() {
    let result = import_lens_daemon::document::analyze_imports(
        "App.js",
        "import { useState } from 'react';\nexport const App = () => <div />;\n",
    );

    // CURRENT (buggy) behavior: the whole document fails to analyze.
    assert!(result.is_err(), "got: {result:?}");

    // Control: the same source as .jsx analyzes fine.
    let control = import_lens_daemon::document::analyze_imports(
        "App.jsx",
        "import { useState } from 'react';\nexport const App = () => <div />;\n",
    );
    assert_eq!(control.expect("jsx should analyze").len(), 1);
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
