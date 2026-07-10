use import_lens_daemon::pipeline::{
    bundle::{BundledModules, bundle_reachable_modules, bundle_reachable_modules_with_metadata},
    graph::build_module_graph,
    minify::{minify_source, minify_source_with_markers},
    reachability::reachable_exports,
};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use std::{
    fs,
    path::{Path, PathBuf},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-bundle")
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
        !parsed.panicked && !parsed.diagnostics.has_errors(),
        "generated source should parse cleanly: {source}"
    );
}

fn contribution_basenames(bundled: &BundledModules) -> Vec<String> {
    bundled
        .contributions
        .iter()
        .map(|contribution| {
            Path::new(&contribution.path)
                .file_name()
                .expect("contribution path should have a file name")
                .to_string_lossy()
                .to_string()
        })
        .collect()
}

/// Every `__il_`-prefixed identifier the bundle reads must be declared by some
/// included module. An unresolved one means the bundler pruned a definition it
/// still references, which silently moves the size estimate.
fn assert_no_dangling_il_bindings(source: &str) {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    let semantic = SemanticBuilder::new().build(&parsed.program);

    let mut dangling = semantic
        .semantic
        .scoping()
        .root_unresolved_references()
        .iter()
        .map(|(name, _)| name.to_string())
        .filter(|name| name.starts_with("__il_"))
        .collect::<Vec<_>>();
    dangling.sort();
    dangling.dedup();

    assert!(
        dangling.is_empty(),
        "bundle references undeclared bindings {dangling:?}:\n{source}"
    );
}

fn assert_semantic_valid(source: &str) {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();

    assert!(
        !parsed.panicked && !parsed.diagnostics.has_errors(),
        "generated source should parse cleanly: {source}"
    );

    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&parsed.program);
    assert!(
        !semantic.diagnostics.has_errors(),
        "generated source should pass semantic checks: {source}\nerrors: {:?}",
        semantic.diagnostics.errors().collect::<Vec<_>>()
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
fn bundle_does_not_emit_import_lens_usage_markers() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const value = 1;");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");
    let minified = minify_source(&bundled, false).expect("reachable bundle should minify");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(!bundled.contains("__importLensUse"), "{bundled}");
    assert!(!minified.contains("__importLensUse"), "{minified}");
}

#[test]
fn minify_source_removes_whitespace_and_preserves_parseability() {
    let minified = minify_source("const value = 1 + 1;\nconsole.log(value);\n", false)
        .expect("source should minify");

    assert!(minified.len() < "const value = 1 + 1;\nconsole.log(value);\n".len());
    assert_parseable(&minified);
}

#[test]
fn bundle_wraps_anonymous_default_class_with_extends() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export class Base { value() { return 1; } }\n\
         export default class extends Base { extra() { return 2; } }",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["default".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");
    let minified = minify_source(&bundled, false);

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(
        minified.is_ok(),
        "anonymous default class with extends must produce a parseable bundle, got:\n{bundled}"
    );
}

#[test]
fn bundle_wraps_anonymous_default_generator() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export default function* () { yield 1; }",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["default".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");
    let minified = minify_source(&bundled, false);

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(
        minified.is_ok(),
        "anonymous default generator must produce a parseable bundle, got:\n{bundled}"
    );
}

#[test]
fn bundle_keeps_named_default_function_as_declaration() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export default function loadThing() { return 1; }",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["default".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");
    let minified = minify_source(&bundled, false);

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(
        bundled.contains("loadThing"),
        "named default fn must be retained: {bundled}"
    );
    assert!(
        minified.is_ok(),
        "named default function must remain a valid declaration, got:\n{bundled}"
    );
}

#[test]
fn bundle_keeps_only_reachable_bindings_from_imported_modules() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { used } from './dep.js';\nexport const value = used;",
    );
    write_source(
        &root,
        "dep.js",
        "export const used = 1;\nexport const unused = 'large unused payload';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.contains("__il_m1_used"), "{bundled}");
    assert!(!bundled.contains("__il_m1_unused"), "{bundled}");
    assert!(!bundled.contains("large unused payload"), "{bundled}");
    assert_parseable(&bundled);
}

#[test]
fn bundle_excludes_imports_used_only_by_unreachable_exports() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { small } from './small.js';\nimport { huge } from './huge.js';\nexport const used = small;\nexport const unused = huge;",
    );
    write_source(&root, "small.js", "export const small = 'small payload';");
    write_source(
        &root,
        "huge.js",
        "export const huge = 'large unused payload';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.source.contains("small payload"), "{bundled:?}");
    assert!(
        !bundled.source.contains("large unused payload"),
        "{bundled:?}"
    );
    assert!(
        !bundled
            .contributions
            .iter()
            .any(|module| module.path.ends_with("huge.js")),
        "{bundled:?}"
    );
    assert_parseable(&bundled.source);
}

#[test]
fn bundle_keeps_imports_referenced_by_reachable_local_helpers() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { helper } from './helper.js';\nexport const used = helper();\nexport const unused = 'unused';",
    );
    write_source(
        &root,
        "helper.js",
        "import { payload } from './payload.js';\nconst local = payload;\nexport function helper() { return local; }",
    );
    write_source(
        &root,
        "payload.js",
        "export const payload = 'required payload';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.contains("required payload"), "{bundled}");
    assert!(!bundled.contains("export const unused"), "{bundled}");
    assert_parseable(&bundled);
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

    assert!(bundled.starts_with("import __il_ext0_default from 'react';\nimport * as __il_ext0_ns from 'react';\nimport { forwardRef as __il_ext0_forwardRef, useState as __il_ext0_useState } from 'react';\n"), "{bundled}");
    // Both the direct `forwardRef` local and the `fr` alias collapse onto the
    // same canonical binding because they import the same external symbol.
    assert!(bundled.contains("__il_ext0_forwardRef("), "{bundled}");

    // Ensure the AST parses without any redeclaration errors from oxc
    assert_parseable(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_renames_colliding_external_import_locals() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import a from './m1.js';\nimport b from './m2.js';\nexport const value = [a, b];",
    );
    write_source(
        &root,
        "m1.js",
        "import shared from 'ext-one';\nexport default shared;",
    );
    write_source(
        &root,
        "m2.js",
        "import shared from 'ext-two';\nexport default shared;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    assert!(
        bundled.source.contains("from 'ext-one'"),
        "{}",
        bundled.source
    );
    assert!(
        bundled.source.contains("from 'ext-two'"),
        "{}",
        bundled.source
    );
    assert_semantic_valid(&bundled.source);
    let minified = minify_source_with_markers(&bundled.minifier_source, false)
        .expect("bundle with colliding external locals should minify");
    assert!(!minified.is_empty());

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_preserves_unicode_strings_and_comments() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export { value } from './unicode.js';");
    write_source(
        &root,
        "unicode.js",
        "const word = \"café ☕\";\nconst note = `emoji 🚀`;\n/* привет */\nexport const value = `${word}:${note}`;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    assert!(bundled.contains("café ☕"), "{bundled}");
    assert!(bundled.contains("emoji 🚀"), "{bundled}");
    assert!(bundled.contains("привет"), "{bundled}");
    assert_parseable(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_removes_no_semicolon_export_specifier_without_dropping_following_code() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export { value } from './lib.js';");
    write_source(
        &root,
        "lib.js",
        "export {}\nconst side = 1;\nexport const value = side;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    assert!(bundled.contains("__il_m1_side = 1"), "{bundled}");
    assert!(
        bundled.contains("__il_m1_value = __il_m1_side"),
        "{bundled}"
    );
    assert!(!bundled.contains("export {}"), "{bundled}");
    assert_parseable(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_renames_destructured_bindings_to_avoid_collisions() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export { left } from './left.js';\nexport { right } from './right.js';",
    );
    write_source(
        &root,
        "left.js",
        "const [value, ...rest] = [1, 2];\nexport const left = value + rest.length;",
    );
    write_source(
        &root,
        "right.js",
        "const { value, ...rest } = { value: 2, extra: 3 };\nexport const right = value + rest.extra;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["left".to_owned(), "right".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    assert!(bundled.contains("__il_m1_value"), "{bundled}");
    assert!(bundled.contains("__il_m2_value"), "{bundled}");
    assert_semantic_valid(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_preserves_object_shorthand_and_string_literals_when_renaming() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export { result } from './lib.js';");
    write_source(
        &root,
        "lib.js",
        "const source = { value: 1 };\nconst { value } = source;\nconst text = \"value\";\nconst object = { value, text };\nexport const result = object.value;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["result".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    assert!(bundled.contains("{ value: __il_m1_value }"), "{bundled}");
    assert!(bundled.contains("\"value\""), "{bundled}");
    assert!(!bundled.contains("\"__il_m1_value\""), "{bundled}");
    assert_semantic_valid(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_namespace_reexport_does_not_emit_marker_for_missing_entry_binding() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * as mod from './dep.js';");
    write_source(
        &root,
        "dep.js",
        "export const value = 1;\nexport const other = 2;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["mod".to_owned()], false);
    let bundled =
        bundle_reachable_modules(&graph, &reachable).expect("reachable modules should bundle");

    assert!(
        !bundled.contains("__il_m0_mod as __importLensUse"),
        "{bundled}"
    );
    assert_semantic_valid(&bundled);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn reachability_follows_star_export_to_reexport_chains() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './b.js';");
    write_source(&root, "b.js", "export { x } from './c.js';");
    write_source(
        &root,
        "c.js",
        "export const x = 1;\nexport const y = 'HEAVY_UNUSED_PAYLOAD';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["x".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.source.contains("__il_m2_x"), "{}", bundled.source);
    assert!(
        !bundled.source.contains("HEAVY_UNUSED_PAYLOAD"),
        "{}",
        bundled.source
    );
}

#[test]
fn reachability_follows_nested_star_export_chains() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './b.js';");
    write_source(&root, "b.js", "export * from './c.js';");
    write_source(
        &root,
        "c.js",
        "export const x = 1;\nexport const y = 'HEAVY_UNUSED_PAYLOAD';",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["x".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(bundled.source.contains("__il_m2_x"), "{}", bundled.source);
    assert!(
        !bundled.source.contains("HEAVY_UNUSED_PAYLOAD"),
        "{}",
        bundled.source
    );
}

#[test]
fn bundle_survives_star_export_cycles_without_stack_overflow() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { x } from './a.js';\nexport const value = x;",
    );
    write_source(&root, "a.js", "export * from './b.js';");
    write_source(&root, "b.js", "export * from './a.js';");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("cyclic star exports should still bundle");

    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
    assert!(
        bundled.source.contains("__il_m0_value"),
        "{}",
        bundled.source
    );
}

#[test]
fn bundle_imported_then_exported_binding_marker_references_target_binding() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { value } from './dep.js';\nexport { value };",
    );
    write_source(&root, "dep.js", "export const value = 1;");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["value".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    assert!(
        !bundled
            .minifier_source
            .contains("__il_m0_value as __importLensUse"),
        "{}",
        bundled.minifier_source
    );
    assert!(
        bundled
            .minifier_source
            .contains("__il_m1_value as __importLensUse"),
        "{}",
        bundled.minifier_source
    );
    assert_semantic_valid(&bundled.minifier_source);
    fs::remove_dir_all(root).expect("temp bundle workspace should be removed");
}

#[test]
fn bundle_distinguishes_non_ascii_identifiers_that_sanitize_alike() {
    let root = temp_workspace();
    // `café` and `cafÉ` differ only in a non-ASCII byte; sanitize_identifier maps
    // both non-ASCII bytes to '_', so a naive scheme collides them.
    write_source(
        &root,
        "entry.js",
        "export const caf\u{e9} = 1;\nexport const caf\u{c9} = 2;",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &[], true);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");

    assert_semantic_valid(&bundled.source);
    let minified = minify_source_with_markers(&bundled.minifier_source, false)
        .expect("distinct non-ASCII identifiers must not collide into one binding");

    fs::remove_dir_all(root).expect("cleanup");
    assert!(!minified.is_empty());
}

#[test]
fn bundle_indirect_reexport_does_not_drag_unrelated_imports() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import { small } from "./small.js";
import { huge } from "./huge.js";
export const used = small;
export const unusedBranch = huge;
export { typed } from "./leaf.js";
"#,
    );
    write_source(&root, "small.js", "export const small = 'S';\n");
    write_source(&root, "huge.js", "export const huge = 'H';\n");
    write_source(&root, "leaf.js", "export const typed = 2;\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["typed".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let files = contribution_basenames(&bundled);

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        files.contains(&"leaf.js".to_owned()),
        "re-export source must be bundled: {files:?}"
    );
    assert!(
        !files.contains(&"huge.js".to_owned()),
        "unreachable import was bundled: {files:?}"
    );
    assert!(
        !files.contains(&"small.js".to_owned()),
        "unreachable import was bundled: {files:?}"
    );
}

#[test]
fn bundle_star_export_does_not_drag_unrelated_imports() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import { noise } from "./noise.js";
export const localNoise = noise;
export * from "./leaf.js";
"#,
    );
    write_source(&root, "noise.js", "export const noise = 'N';\n");
    write_source(&root, "leaf.js", "export const typed = 2;\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["typed".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let files = contribution_basenames(&bundled);

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        files.contains(&"leaf.js".to_owned()),
        "star-export source must be bundled: {files:?}"
    );
    assert!(
        !files.contains(&"noise.js".to_owned()),
        "unreachable import was bundled: {files:?}"
    );
}

#[test]
fn bundle_follows_reexport_chain_behind_named_import() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import { typed } from "./mid.js";
export const wrapped = typed;
"#,
    );
    write_source(
        &root,
        "mid.js",
        r#"import { noise } from "./noise.js";
export const midNoise = noise;
export { typed } from "./leaf.js";
"#,
    );
    write_source(&root, "noise.js", "export const noise = 'N';\n");
    write_source(&root, "leaf.js", "export const typed = 2;\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["wrapped".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let files = contribution_basenames(&bundled);

    fs::remove_dir_all(root).expect("cleanup");
    // Dropping leaf.js here is an *under*-count: the bundle would reference a
    // binding that no included module defines.
    assert!(
        files.contains(&"leaf.js".to_owned()),
        "re-export source behind a named import must be bundled: {files:?}"
    );
    assert!(
        !files.contains(&"noise.js".to_owned()),
        "unreachable import was bundled: {files:?}"
    );
    assert_semantic_valid(&bundled.source);
}

#[test]
fn bundle_follows_reexport_chains_marked_by_separate_imports() {
    // Two import statements hit hub.js one after the other, each making a
    // different re-exported name reachable. The second visit arrives when the
    // module was already processed, so this locks the re-visit change
    // detection: a newly reachable symbol must still get its chain followed.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import { a } from "./hub.js";
import { b } from "./hub.js";
export const outA = a;
export const outB = b;
"#,
    );
    write_source(
        &root,
        "hub.js",
        r#"export { a } from "./aa.js";
export { b } from "./bb.js";
"#,
    );
    write_source(&root, "aa.js", "export const a = 'A';\n");
    write_source(&root, "bb.js", "export const b = 'B';\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["outA".to_owned(), "outB".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let files = contribution_basenames(&bundled);

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        files.contains(&"aa.js".to_owned()),
        "first re-export chain must be bundled: {files:?}"
    );
    assert!(
        files.contains(&"bb.js".to_owned()),
        "second re-export chain must be bundled: {files:?}"
    );
    assert_semantic_valid(&bundled.source);
}

#[test]
fn bundle_keeps_import_referenced_only_by_top_level_side_effect_statement() {
    // `sideEffect(foo)` binds nothing, so it is not a binding dependency of any
    // export, yet the rewriter keeps it and the minifier cannot drop it. Its
    // import must survive in both a re-export barrel and a local-export module,
    // or the bundle references an undeclared `foo`.
    for (label, hub_source) in [
        (
            "barrel",
            r#"import { foo } from "./other.js";
export { x } from "./leaf.js";
sideEffect(foo);
"#,
        ),
        (
            "local export",
            r#"import { foo } from "./other.js";
export const x = 1;
sideEffect(foo);
"#,
        ),
    ] {
        let root = temp_workspace();
        write_source(&root, "entry.js", "export { x } from \"./hub.js\";\n");
        write_source(&root, "hub.js", hub_source);
        write_source(&root, "other.js", "export const foo = 'F';\n");
        write_source(&root, "leaf.js", "export const x = 1;\n");

        let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
        let reachable = reachable_exports(&graph, &["x".to_owned()], false);
        let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
            .expect("reachable modules should bundle");
        let files = contribution_basenames(&bundled);
        let source = bundled.source.clone();

        fs::remove_dir_all(root).expect("cleanup");
        assert!(
            files.contains(&"other.js".to_owned()),
            "{label}: import read by a side-effect statement was pruned: {files:?}\n{source}"
        );
        assert!(
            source.contains("sideEffect("),
            "{label}: side-effect statement should survive: {source}"
        );
    }
}

#[test]
fn bundle_keeps_all_imports_of_side_effect_only_module() {
    // Guard for the conservative fallback: a module reached purely for side
    // effects has no reachable symbols, so all of its static imports stay.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import "./effect.js";
export const used = 1;
"#,
    );
    write_source(
        &root,
        "effect.js",
        r#"import { dep } from "./dep.js";
globalThis.__importLensEffect = dep;
"#,
    );
    write_source(&root, "dep.js", "export const dep = 'D';\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let files = contribution_basenames(&bundled);

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        files.contains(&"effect.js".to_owned()),
        "side-effect module must be bundled: {files:?}"
    );
    assert!(
        files.contains(&"dep.js".to_owned()),
        "side-effect module's import must be bundled: {files:?}"
    );
}

#[test]
fn bundle_declares_namespace_object_for_escaping_namespace_import() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import * as helpers from "./helpers.js";
export const used = Object.keys(helpers);
"#,
    );
    write_source(
        &root,
        "helpers.js",
        "export const alpha = 1;\nexport const beta = 2;\n",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        source.contains("__il_m1_namespace = {"),
        "escaping namespace must be materialized: {source}"
    );
    assert!(
        source.contains("alpha: __il_m1_alpha"),
        "namespace object must expose alpha: {source}"
    );
    assert!(
        source.contains("beta: __il_m1_beta"),
        "escaping namespace keeps every export: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}

#[test]
fn bundle_inlines_static_namespace_member_access_and_shakes_the_rest() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import * as helpers from "./helpers.js";
export const used = helpers.alpha(1);
"#,
    );
    write_source(
        &root,
        "helpers.js",
        r#"export const alpha = (n) => n + 1;
export const beta = (n) => n + 2;
export const HUGE = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
"#,
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        source.contains("__il_m1_alpha(1)"),
        "ns.alpha must inline to the target binding: {source}"
    );
    assert!(
        !source.contains("__il_m1_namespace"),
        "a non-escaping namespace must not be materialized: {source}"
    );
    assert!(
        !source.contains("__il_m1_beta"),
        "unaccessed export must be shaken out: {source}"
    );
    assert!(
        !source.contains("xxxxxxxx"),
        "unaccessed payload must be shaken out: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}

#[test]
fn bundle_escapes_namespace_used_as_a_value_or_computed() {
    for (label, entry_source) in [
        ("value", "export const used = Object.keys(helpers);"),
        (
            "computed",
            "const k = 'alpha';\nexport const used = helpers[k];",
        ),
        ("optional", "export const used = helpers?.alpha;"),
        ("unknown property", "export const used = helpers.nope;"),
    ] {
        let root = temp_workspace();
        write_source(
            &root,
            "entry.js",
            &format!("import * as helpers from \"./helpers.js\";\n{entry_source}\n"),
        );
        write_source(
            &root,
            "helpers.js",
            "export const alpha = 1;\nexport const beta = 2;\n",
        );

        let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
        let reachable = reachable_exports(&graph, &["used".to_owned()], false);
        let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
            .expect("reachable modules should bundle");
        let source = bundled.source.clone();

        fs::remove_dir_all(root).expect("cleanup");
        assert!(
            source.contains("__il_m1_namespace = {"),
            "{label}: escaping namespace must be materialized: {source}"
        );
        assert!(
            source.contains("__il_m1_beta"),
            "{label}: escaping namespace keeps every export: {source}"
        );
        assert_no_dangling_il_bindings(&source);
        assert_semantic_valid(&source);
    }
}

#[test]
fn bundle_inlines_namespace_member_reaching_through_a_reexport() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import * as ns from "./barrel.js";
export const used = ns.alpha;
"#,
    );
    write_source(&root, "barrel.js", "export { alpha } from \"./leaf.js\";\n");
    write_source(
        &root,
        "leaf.js",
        "export const alpha = 1;\nexport const beta = 2;\n",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let files = contribution_basenames(&bundled);
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(files.contains(&"leaf.js".to_owned()), "{files:?}");
    assert!(
        !source.contains("__il_m2_beta"),
        "re-export chain must still shake unaccessed names: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}

#[test]
fn bundle_declares_namespace_object_for_namespace_reexport_barrel() {
    // `export * as Leaf from "./leaf.js"` exposes leaf's namespace under the
    // name Leaf. There is no export literally called `*` to resolve, so the
    // member is leaf's namespace object -- which must itself be declared.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import * as nodes from "./barrel.js";
export const used = Object.keys(nodes);
"#,
    );
    write_source(&root, "barrel.js", "export * as Leaf from \"./leaf.js\";\n");
    write_source(
        &root,
        "leaf.js",
        "export const name = 'Leaf';\nexport const parse = () => 1;\n",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        source.contains("Leaf: __il_m2_namespace"),
        "barrel member must be the leaf namespace object: {source}"
    );
    assert!(
        source.contains("__il_m2_namespace = {"),
        "the leaf namespace object must be declared: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}

#[test]
fn bundle_declares_namespace_reexport_forwarded_through_a_star_export() {
    // a.js reaches `export * as X` only through `export *`. The namespace object
    // for a.js names X, so x.js's namespace must be declared too -- deriving the
    // child set from `reexports` alone silently misses this.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import * as nsA from "./a.js";
export const used = Object.keys(nsA);
"#,
    );
    write_source(&root, "a.js", "export * from \"./barrel.js\";\n");
    write_source(&root, "barrel.js", "export * as X from \"./x.js\";\n");
    write_source(&root, "x.js", "export const v = 1;\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        source.contains("__il_m3_namespace = {"),
        "star-forwarded namespace re-export must be declared: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}

#[test]
fn bundle_reports_one_contribution_row_per_module_path() {
    // A materialized namespace target also emits its own rewritten source. Two
    // rows for one path would list the file twice in the size breakdown.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import * as helpers from "./helpers.js";
export const used = Object.keys(helpers);
"#,
    );
    write_source(
        &root,
        "helpers.js",
        "export const alpha = 1;\nexport const beta = 2;\n",
    );

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let paths = bundled
        .contributions
        .iter()
        .map(|contribution| contribution.path.clone())
        .collect::<Vec<_>>();
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();

    fs::remove_dir_all(root).expect("cleanup");
    assert_eq!(
        paths.len(),
        unique.len(),
        "each module path must appear once in the breakdown: {paths:?}"
    );
}

#[test]
fn bundle_omits_namespace_object_for_unreached_namespace_reexport() {
    // The entry re-exports `Dead` as a namespace but the request only reaches
    // `used`. Materializing Dead's namespace would name every export of
    // dead.js and keep the whole module alive for nothing.
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"export * as Dead from "./dead.js";
export { used } from "./live.js";
"#,
    );
    write_source(
        &root,
        "dead.js",
        "export const payload = 'zzzzzzzzzzzzzzzzzzzzzzzz';\n",
    );
    write_source(&root, "live.js", "export const used = 1;\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        !source.contains("_namespace = {"),
        "unreached namespace re-export must not be materialized: {source}"
    );
    assert!(
        !source.contains("zzzzzzzz"),
        "unreached namespace re-export must not drag its payload: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}

#[test]
fn bundle_named_import_of_namespace_reexport_resolves_to_namespace_object() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        r#"import { Leaf } from "./barrel.js";
export const used = Leaf;
"#,
    );
    write_source(&root, "barrel.js", "export * as Leaf from \"./leaf.js\";\n");
    write_source(&root, "leaf.js", "export const name = 'Leaf';\n");

    let graph = build_module_graph(&root.join("entry.js")).expect("graph should be built");
    let reachable = reachable_exports(&graph, &["used".to_owned()], false);
    let bundled = bundle_reachable_modules_with_metadata(&graph, &reachable)
        .expect("reachable modules should bundle");
    let source = bundled.source.clone();

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        source.contains("__il_m0_used = __il_m2_namespace"),
        "named import of a namespace re-export must bind the namespace: {source}"
    );
    assert_no_dangling_il_bindings(&source);
    assert_semantic_valid(&source);
}
