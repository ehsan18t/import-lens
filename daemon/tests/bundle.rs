use import_lens_daemon::pipeline::{
    bundle::bundle_reachable_modules, graph::build_module_graph, minify::minify_source,
    reachability::reachable_exports,
};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
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
    let path = std::env::temp_dir().join(format!("import-lens-bundle-{suffix}"));
    fs::create_dir_all(&path).expect("temp bundle workspace should be created");
    path
}

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn assert_parseable(source: &str) {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();

    assert!(
        !parsed.panicked && parsed.errors.is_empty(),
        "generated source should parse cleanly: {source}"
    );
}

#[test]
fn bundle_renames_module_scoped_bindings_to_avoid_collisions() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export { left } from './left.js';\nexport { right } from './right.js';",
    );
    write_source(
        &root,
        "left.js",
        "const value = 1;\nexport const left = value;",
    );
    write_source(
        &root,
        "right.js",
        "const value = 2;\nexport const right = value;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["left".to_owned(), "right".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    assert!(bundled.contains("__il_m1_value"));
    assert!(bundled.contains("__il_m2_value"));
    assert_parseable(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn minify_source_removes_whitespace_and_preserves_parseability() {
    let minified =
        minify_source("const value = 1 + 1;\nconsole.log(value);\n").expect("source should minify");

    assert!(minified.len() < "const value = 1 + 1;\nconsole.log(value);\n".len());
    assert_parseable(&minified);
}

#[test]
fn bundle_hoists_and_deduplicates_external_imports() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export { left } from './left.js';\nexport { right } from './right.js';",
    );
    write_source(
        &root,
        "left.js",
        "import React from 'react';\nimport { forwardRef } from 'react';\nexport const left = forwardRef(() => React.createElement('div'));",
    );
    write_source(
        &root,
        "right.js",
        "import { forwardRef, useState } from 'react';\nimport * as ReactNamespace from 'react';\nimport { forwardRef as fr } from 'react';\nimport 'react';\nexport const right = fr(() => ReactNamespace.createElement('div'));",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["left".to_owned(), "right".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    println!("BUNDLED:\n{}", bundled);
    assert!(bundled.starts_with("import React from 'react';\nimport * as ReactNamespace from 'react';\nimport { forwardRef, forwardRef as fr, useState } from 'react';\n"));

    // Ensure the AST parses without any redeclaration errors from oxc
    assert_parseable(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}
