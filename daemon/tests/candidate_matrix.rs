//! Qualification construct matrix for the Rolldown candidate engine
//! (bundler redesign spec §10.2). Every row drives the engine through the
//! Import Lens contract only; no test may reach into Rolldown types.

use import_lens_daemon::engine::{
    BundleArtifact, BundleEntry, BundleFailure, BundlePurpose, BundleRequest, BundleSelection,
    ImportRuntime, RolldownEngine, SideEffectsMode,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-candidate-matrix")
}

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn assert_artifact_valid(artifact: &BundleArtifact) {
    common::assert_parseable(&artifact.code);
    common::assert_semantic_valid(&artifact.code);
    common::assert_no_dangling_il_bindings(&artifact.code);
}

fn contribution_basenames(artifact: &BundleArtifact) -> Vec<String> {
    artifact
        .contributions
        .iter()
        .map(|contribution| {
            contribution
                .path
                .file_name()
                .expect("contribution path should have a file name")
                .to_string_lossy()
                .to_string()
        })
        .collect()
}

fn request(root: &Path, entry: &str, selection: BundleSelection) -> BundleRequest {
    BundleRequest {
        entries: vec![BundleEntry {
            entry_path: root.join(entry),
            package_root: root.to_path_buf(),
            selection,
            reported_side_effects: SideEffectsMode::Unknown,
        }],
        runtime: ImportRuntime::default(),
        purpose: BundlePurpose::ImportSize,
    }
}

async fn run(
    root: &Path,
    entry: &str,
    selection: BundleSelection,
) -> Result<BundleArtifact, BundleFailure> {
    RolldownEngine.bundle(request(root, entry, selection)).await
}

async fn bundle_ok(root: &Path, entry: &str, selection: BundleSelection) -> BundleArtifact {
    let artifact = run(root, entry, selection)
        .await
        .expect("bundle should succeed");
    assert_artifact_valid(&artifact);
    artifact
}

fn named(names: &[&str]) -> BundleSelection {
    BundleSelection::Named(names.iter().map(|name| (*name).to_owned()).collect())
}

// Row 1: local named export.
#[tokio::test]
async fn matrix_01_local_named_export() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const parse = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["parse"])).await;

    assert!(artifact.code.contains("parse"), "{}", artifact.code);
    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 2: local default export requested through the default alias.
#[tokio::test]
async fn matrix_02_local_default_export() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "const v = 1;\nexport default v;");

    let artifact = bundle_ok(&root, "entry.js", BundleSelection::Default).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_default".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 3: imported-then-exported binding pulls the source module in.
#[tokio::test]
async fn matrix_03_imported_then_exported() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { a } from './a.js';\nexport { a };",
    );
    write_source(&root, "a.js", "export const a = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["a"])).await;

    assert!(
        contribution_basenames(&artifact).contains(&"a.js".to_owned()),
        "contributions: {:?}",
        artifact.contributions
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 4: direct named re-export renders the target module.
#[tokio::test]
async fn matrix_04_direct_named_reexport() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export { leaf } from './leaf.js';");
    write_source(&root, "leaf.js", "export const leaf = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["leaf"])).await;

    assert!(
        contribution_basenames(&artifact).contains(&"leaf.js".to_owned()),
        "contributions: {:?}",
        artifact.contributions
    );
    assert!(artifact.code.contains("leaf"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 5: a name reached through a single `export *`.
#[tokio::test]
async fn matrix_05_single_star_reexport() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './leaf.js';");
    write_source(&root, "leaf.js", "export const leaf = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["leaf"])).await;

    assert!(artifact.code.contains("leaf"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 6: chained `export *` renders the providing leaf exactly once.
#[tokio::test]
async fn matrix_06_chained_star_reexport() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './mid.js';");
    write_source(&root, "mid.js", "export * from './leaf.js';");
    write_source(&root, "leaf.js", "export const leaf = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["leaf"])).await;

    let leaf_contributions = contribution_basenames(&artifact)
        .into_iter()
        .filter(|name| name == "leaf.js")
        .count();
    assert_eq!(
        leaf_contributions, 1,
        "contributions: {:?}",
        artifact.contributions
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 7: two star providers exporting the same name is ambiguous and must
// surface as a typed failure, never a silently chosen provider.
#[tokio::test]
async fn matrix_07_ambiguous_star_providers() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export * from './a.js';\nexport * from './b.js';",
    );
    write_source(&root, "a.js", "export const x = 1;");
    write_source(&root, "b.js", "export const x = 2;");

    let failure = run(&root, "entry.js", named(&["x"]))
        .await
        .expect_err("ambiguous star providers should fail");

    assert_eq!(failure.stage, "ambiguous_export", "{failure:?}");
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 8: `export * as ns` produces a valid namespace object.
#[tokio::test]
async fn matrix_08_star_as_namespace_reexport() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * as ns from './leaf.js';");
    write_source(&root, "leaf.js", "export const a = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["ns"])).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 9: a namespace re-export forwarded through `export *`.
#[tokio::test]
async fn matrix_09_forwarded_namespace_reexport() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from './mid.js';");
    write_source(&root, "mid.js", "export * as ns from './leaf.js';");
    write_source(&root, "leaf.js", "export const a = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["ns"])).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 10: static namespace member access retains only the touched export.
#[tokio::test]
async fn matrix_10_namespace_static_read() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import * as ns from './leaf.js';\nexport const r = ns.a;",
    );
    write_source(&root, "leaf.js", "export const a = 1;\nexport const b = 2;");

    let artifact = bundle_ok(&root, "entry.js", named(&["r"])).await;

    assert!(!artifact.code.contains("= 2"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 11: computed namespace access keeps the whole leaf alive.
#[tokio::test]
async fn matrix_11_namespace_computed_read() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import * as ns from './leaf.js';\nconst key = 'a';\nexport const r = ns[key];",
    );
    write_source(&root, "leaf.js", "export const a = 1;\nexport const b = 2;");

    let artifact = bundle_ok(&root, "entry.js", named(&["r"])).await;

    assert!(artifact.code.contains("= 2"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 12: optional-chained namespace access stays valid.
#[tokio::test]
async fn matrix_12_namespace_optional_read() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import * as ns from './leaf.js';\nexport const r = ns?.a;",
    );
    write_source(&root, "leaf.js", "export const a = 1;");

    bundle_ok(&root, "entry.js", named(&["r"])).await;
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 13: a namespace escaping through a closure is materialized.
#[tokio::test]
async fn matrix_13_escaping_namespace() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import * as ns from './leaf.js';\nexport const grab = () => ns;",
    );
    write_source(&root, "leaf.js", "export const a = 1;");

    bundle_ok(&root, "entry.js", named(&["grab"])).await;
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 14: an escaping namespace over an EMPTY module — the §2.2 construct
// the old engine emits dangling `__il_*_namespace` references for.
#[tokio::test]
async fn matrix_14_escaping_namespace_over_empty_module() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import * as ns from './empty.js';\nexport const grab = () => ns;",
    );
    write_source(&root, "empty.js", "");

    bundle_ok(&root, "entry.js", named(&["grab"])).await;
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 15: string-literal export names resolve through the quoted form.
#[tokio::test]
async fn matrix_15_string_literal_export_name() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "const v = 1;\nexport { v as 'a-b' };");

    let artifact = bundle_ok(&root, "entry.js", named(&["a-b"])).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 16: a bare side-effect import keeps the imported module alive.
#[tokio::test]
async fn matrix_16_side_effect_only_import() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "import './fx.js';\nexport const x = 1;");
    write_source(&root, "fx.js", "globalThis.__p = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("__p"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 17: a pure unused declaration is tree-shaken out.
#[tokio::test]
async fn matrix_17_pure_unused_declaration() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "const dead = 1;\nexport const parse = 2;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["parse"])).await;

    assert!(!artifact.code.contains("dead"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 18: an effectful top-level statement keeps itself and the value it
// reads (the retained-import case fixed at f4460fa in the old engine).
// Rolldown constant-inlines the imported const at link time, so foo.js
// legitimately renders zero bytes; the durable guarantees are the retained
// call and foo.js staying in the freshness fingerprint set.
#[tokio::test]
async fn matrix_18_effectful_unused_non_export() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { foo } from './foo.js';\nsideEffect(foo);\nexport const x = 1;",
    );
    write_source(&root, "foo.js", "export const foo = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("sideEffect("), "{}", artifact.code);
    assert!(
        artifact
            .loaded_paths
            .iter()
            .any(|path| path.ends_with("foo.js")),
        "loaded paths: {:?}",
        artifact.loaded_paths
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 19: an unrequested export with an effectful initializer keeps the
// initializer and its dependency (the deleted-initializer §2.2 case).
#[tokio::test]
async fn matrix_19_effectful_initializer_of_unrequested_export() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { compute } from './compute.js';\nimport { dep } from './dep.js';\n\
         export const unused = compute(dep);\nexport const wanted = 1;",
    );
    write_source(
        &root,
        "compute.js",
        "export const compute = (value) => {\n  globalThis.__computed = value;\n  return value;\n};",
    );
    write_source(&root, "dep.js", "export const dep = 2;");

    let artifact = bundle_ok(&root, "entry.js", named(&["wanted"])).await;

    assert!(artifact.code.contains("__computed"), "{}", artifact.code);
    // dep's const is constant-inlined into the retained call, so it renders
    // zero bytes but must stay a fingerprinted input.
    assert!(
        artifact
            .loaded_paths
            .iter()
            .any(|path| path.ends_with("dep.js")),
        "loaded paths: {:?}",
        artifact.loaded_paths
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 20: a binding-less top-level call keeps the called import's module.
#[tokio::test]
async fn matrix_20_binding_less_top_level_statement() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { f } from './f.js';\nf();\nexport const x = 1;",
    );
    write_source(
        &root,
        "f.js",
        "export const f = () => {\n  globalThis.__called = true;\n};",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(
        contribution_basenames(&artifact).contains(&"f.js".to_owned()),
        "contributions: {:?}",
        artifact.contributions
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 21: an import cycle builds and renders each module exactly once.
#[tokio::test]
async fn matrix_21_import_cycle() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export { a } from './a.js';");
    write_source(
        &root,
        "a.js",
        "import { b } from './b.js';\nexport const a = () => b;",
    );
    write_source(
        &root,
        "b.js",
        "import { a } from './a.js';\nexport const b = () => a;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["a"])).await;

    let basenames = contribution_basenames(&artifact);
    for module in ["a.js", "b.js"] {
        assert_eq!(
            basenames.iter().filter(|name| *name == module).count(),
            1,
            "contributions: {:?}",
            artifact.contributions
        );
    }
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 22: a shared diamond dependency is rendered exactly once. The shared
// module exports a function because a trivial const would be inlined and
// render zero bytes, which cannot prove the dedup.
#[tokio::test]
async fn matrix_22_shared_diamond() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { left } from './a.js';\nimport { right } from './b.js';\nexport const x = left + right;",
    );
    write_source(
        &root,
        "a.js",
        "import { shared } from './shared.js';\nexport const left = shared(1);",
    );
    write_source(
        &root,
        "b.js",
        "import { shared } from './shared.js';\nexport const right = shared(2);",
    );
    write_source(
        &root,
        "shared.js",
        "export const shared = (value) => value + 1;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    let shared_contributions = contribution_basenames(&artifact)
        .into_iter()
        .filter(|name| name == "shared.js")
        .count();
    assert_eq!(
        shared_contributions, 1,
        "contributions: {:?}\ncode:\n{}",
        artifact.contributions, artifact.code
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 23: a Node builtin stays an external import boundary with a
// structured diagnostic, not a failure.
#[tokio::test]
async fn matrix_23_builtin_external_import() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import fs from \"node:fs\";\nexport const x = fs;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("node:fs"), "{}", artifact.code);
    assert!(
        artifact
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "external"
                && diagnostic.message.contains("node:fs")),
        "diagnostics: {:?}",
        artifact.diagnostics
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 24: a named re-export from an unresolvable package keeps the external
// boundary in the output with a structured diagnostic — never a silently
// empty chunk (§2.2 case). Rolldown 1.1.5 externalizes the unresolved
// specifier and reports it as a warning rather than failing the build.
#[tokio::test]
async fn matrix_24_external_named_reexport() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "export { thing } from \"unresolvable-pkg\";",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["thing"])).await;

    assert!(
        artifact.code.contains("unresolvable-pkg"),
        "{}",
        artifact.code
    );
    assert!(
        artifact
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unresolvable-pkg")),
        "diagnostics: {:?}",
        artifact.diagnostics
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 25: a star re-export from an unresolvable package follows the same
// policy as row 24.
#[tokio::test]
async fn matrix_25_external_star_reexport() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export * from \"unresolvable-pkg\";");

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(
        artifact.code.contains("unresolvable-pkg"),
        "{}",
        artifact.code
    );
    assert!(
        artifact
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unresolvable-pkg")),
        "diagnostics: {:?}",
        artifact.diagnostics
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 26: CommonJS interop through a default import.
#[tokio::test]
async fn matrix_26_cjs_interop() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import pkg from './leaf.cjs';\nexport const fn = pkg.fn;",
    );
    write_source(
        &root,
        "leaf.cjs",
        "module.exports = { fn() { return 1; } };",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["fn"])).await;

    assert!(
        contribution_basenames(&artifact).contains(&"leaf.cjs".to_owned()),
        "contributions: {:?}",
        artifact.contributions
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 27: CommonJS export shapes surface through export enumeration.
#[tokio::test]
async fn matrix_27_cjs_export_shapes() {
    let root = temp_workspace();
    write_source(
        &root,
        "leaf.cjs",
        "exports.named = 1;\nmodule.exports.x = 2;\nif (globalThis.__cond) {\n  exports.maybe = 3;\n}",
    );

    let exported = RolldownEngine
        .enumerate_exports(root.join("leaf.cjs"), ImportRuntime::default())
        .await
        .expect("cjs enumeration should succeed");

    // Qualification finding (§10.7): Rolldown 1.1.5 exposes a CJS entry's
    // chunk export list as `default` only, even for statically plain
    // assignments — named CJS surfaces come from link-time interop, not
    // enumeration. This is Rolldown's resolution result; it is never
    // augmented by guessing (§8.4).
    assert_eq!(exported.names, vec!["default".to_owned()]);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 28: TS/TSX/JSX/JSON/.mts/.cts inputs transform natively into one
// parseable chunk.
#[tokio::test]
async fn matrix_28_typescript_and_asset_inputs() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.ts",
        "import { widget } from './widget.jsx';\nimport { comp } from './component.tsx';\n\
         import { meta } from './mod.mts';\nimport legacy from './legacy.cts';\n\
         import data from './data.json';\n\
         export const all: unknown[] = [widget, comp, meta, legacy, data];",
    );
    write_source(&root, "widget.jsx", "export const widget = <div />;");
    write_source(
        &root,
        "component.tsx",
        "export const comp: object = <span />;",
    );
    write_source(&root, "mod.mts", "export const meta: number = 1;");
    write_source(
        &root,
        "legacy.cts",
        "module.exports = { legacy: 2 as number };",
    );
    write_source(&root, "data.json", "{\"n\": 1}");
    // The automatic JSX runtime imports react/jsx-runtime; provide a minimal
    // local package so the temp workspace resolves it.
    write_source(
        &root,
        "node_modules/react/package.json",
        "{\"name\":\"react\",\"version\":\"0.0.0\",\"exports\":{\"./jsx-runtime\":\"./jsx-runtime.js\"}}",
    );
    write_source(
        &root,
        "node_modules/react/jsx-runtime.js",
        "export const jsx = (type, props) => ({ type, props });\n\
         export const jsxs = jsx;\nexport const Fragment = Symbol('fragment');",
    );

    let artifact = bundle_ok(&root, "entry.ts", named(&["all"])).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 29: a source-declared `__il_`-prefixed identifier is deconflicted.
#[tokio::test]
async fn matrix_29_il_prefix_collision() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "const __il_entry_0_export_0 = 5;\nexport const x = __il_entry_0_export_0;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_export_0".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 30: a missing requested export is a typed failure, never a guessed
// binding (§12).
#[tokio::test]
async fn matrix_30_missing_export() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const a = 1;");

    let failure = run(&root, "entry.js", named(&["nope"]))
        .await
        .expect_err("missing export should fail");

    assert_eq!(failure.stage, "missing_export", "{failure:?}");
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 31: a syntactically invalid module is a typed parse failure.
#[tokio::test]
async fn matrix_31_parse_failure() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export const = ;");

    let failure = run(&root, "entry.js", named(&["x"]))
        .await
        .expect_err("invalid syntax should fail");

    assert_eq!(failure.stage, "parse", "{failure:?}");
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 32: exceeding the module-count limit is a deterministic typed
// failure, never a partial graph (§7.3).
#[tokio::test]
async fn matrix_32_module_count_limit() {
    let root = temp_workspace();
    // entry.js plus a 2,001-module chain crosses MAX_GRAPH_MODULES (2,000).
    write_source(&root, "entry.js", "export { v } from './mod_0.js';");
    for index in 0..=2000 {
        let source = if index == 2000 {
            "export const v = 0;".to_owned()
        } else {
            format!(
                "import {{ v as p }} from './mod_{}.js';\nexport const v = p + 1;",
                index + 1
            )
        };
        write_source(&root, &format!("mod_{index}.js"), &source);
    }

    let failure = run(&root, "entry.js", named(&["v"]))
        .await
        .expect_err("module count limit should fail");

    assert_eq!(failure.stage, "module_graph_limit", "{failure:?}");
    assert!(failure.message.contains("2000"), "{failure:?}");
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 33: a single module over the per-module source limit is a typed
// failure.
#[tokio::test]
async fn matrix_33_single_module_size_limit() {
    let root = temp_workspace();
    let big = format!("export const big = \"{}\";", "A".repeat(21 * 1024 * 1024));
    write_source(&root, "entry.js", "export { big } from './big.js';");
    write_source(&root, "big.js", &big);

    let failure = run(&root, "entry.js", named(&["big"]))
        .await
        .expect_err("module size limit should fail");

    assert_eq!(failure.stage, "module_graph_limit", "{failure:?}");
    assert!(
        failure.message.contains("module source limit"),
        "{failure:?}"
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 34: total-source limit — a DISTINCT branch from row 33 (the
// MAX_GRAPH_SOURCE_BYTES accumulator, not the per-module cap). It lives in
// `graph_source_limit.rs` rather than here: it shrinks the ceiling through an
// environment override, which is process-wide, so it needs a test binary of its
// own. It runs by default there — no fixture over a megabyte, nothing ignored.

// Row 35: a transitive dynamic import inlines into the single chunk —
// the code-splitting knob works (§6.2/§7.1).
#[tokio::test]
async fn matrix_35_transitive_dynamic_import() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { run } from './dep.js';\nexport const go = run;",
    );
    write_source(
        &root,
        "dep.js",
        "export const run = () => import('./lazy.js');",
    );
    write_source(&root, "lazy.js", "export const lazy = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["go"])).await;

    assert!(artifact.code.contains("lazy"), "{}", artifact.code);
    assert!(
        artifact
            .loaded_paths
            .iter()
            .any(|path| path.ends_with("lazy.js")),
        "loaded paths: {:?}",
        artifact.loaded_paths
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 36: a Full selection measures the complete surface, default included.
#[tokio::test]
async fn matrix_36_full_selection() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "const d = 1;\nexport default d;\nexport const named = 2;",
    );

    let artifact = bundle_ok(&root, "entry.js", BundleSelection::Full).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_namespace".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    assert!(artifact.code.contains("named"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 37: a multi-package request counts a shared transitive dependency
// once (§6.3).
#[tokio::test]
async fn matrix_37_multi_package_request() {
    let root = temp_workspace();
    write_source(
        &root,
        "pkg_a/index.js",
        "import { shared } from '../shared.js';\nexport const a = shared(1);",
    );
    write_source(
        &root,
        "pkg_b/index.js",
        "import { shared } from '../shared.js';\nexport const b = shared(2);",
    );
    write_source(
        &root,
        "shared.js",
        "export const shared = (value) => value + 1;",
    );

    let artifact = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![
                BundleEntry {
                    entry_path: root.join("pkg_a/index.js"),
                    package_root: root.clone(),
                    selection: named(&["a"]),
                    reported_side_effects: SideEffectsMode::Unknown,
                },
                BundleEntry {
                    entry_path: root.join("pkg_b/index.js"),
                    package_root: root.clone(),
                    selection: named(&["b"]),
                    reported_side_effects: SideEffectsMode::Unknown,
                },
            ],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::FileSize,
        })
        .await
        .expect("multi-package bundle should succeed");
    assert_artifact_valid(&artifact);

    let shared_contributions = contribution_basenames(&artifact)
        .into_iter()
        .filter(|name| name == "shared.js")
        .count();
    assert_eq!(
        shared_contributions, 1,
        "contributions: {:?}",
        artifact.contributions
    );
    for alias in ["__il_entry_0_export_0", "__il_entry_1_export_0"] {
        assert!(
            artifact.exported_names.contains(&alias.to_owned()),
            "exported names: {:?}",
            artifact.exported_names
        );
    }
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Rows 38-44 (§7.4): `package.json#sideEffects` interpretation is
// Rolldown's alone; the plugin never overrides it, and matched-path
// reporting never changes retention. Each fixture installs a fake package
// under node_modules and imports it for side effects only.

fn write_side_effect_package(root: &Path, side_effects_field: Option<&str>) {
    let side_effects = side_effects_field
        .map(|value| format!(",\"sideEffects\":{value}"))
        .unwrap_or_default();
    write_source(
        root,
        "node_modules/testpkg/package.json",
        &format!(
            "{{\"name\":\"testpkg\",\"version\":\"0.0.0\",\"main\":\"./index.js\"{side_effects}}}"
        ),
    );
    write_source(root, "entry.js", "import 'testpkg';\nexport const x = 1;");
}

// Row 38: sideEffects:false drops the effect-only module.
#[tokio::test]
async fn matrix_38_side_effects_false() {
    let root = temp_workspace();
    write_side_effect_package(&root, Some("false"));
    write_source(
        &root,
        "node_modules/testpkg/index.js",
        "globalThis.__fx = 1;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(!artifact.code.contains("__fx"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 39: sideEffects:true retains it.
#[tokio::test]
async fn matrix_39_side_effects_true() {
    let root = temp_workspace();
    write_side_effect_package(&root, Some("true"));
    write_source(
        &root,
        "node_modules/testpkg/index.js",
        "globalThis.__fx = 1;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("__fx"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 40: a missing sideEffects field retains it.
#[tokio::test]
async fn matrix_40_side_effects_missing() {
    let root = temp_workspace();
    write_side_effect_package(&root, None);
    write_source(
        &root,
        "node_modules/testpkg/index.js",
        "globalThis.__fx = 1;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("__fx"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 41: an invalid sideEffects value falls back to Rolldown's native
// interpretation (treated like a missing field — retained).
#[tokio::test]
async fn matrix_41_side_effects_invalid() {
    let root = temp_workspace();
    write_side_effect_package(&root, Some("42"));
    write_source(
        &root,
        "node_modules/testpkg/index.js",
        "globalThis.__fx = 1;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("__fx"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

/// Rows 42/43: a string / array `sideEffects` glob keeps matching files and drops
/// the rest.
///
/// These rows were `#[ignore]`d with a recorded qualification finding that Rolldown
/// "never matches sideEffects globs on Windows (backslashed relative paths)". That is
/// false, and both halves of it are refutable: Rolldown matches through `fast_glob`,
/// which uses `std::path::is_separator` and deliberately accepts `\` for a pattern's
/// `/` on Windows; and the old fixture's pattern resolved to `fx.js` at the package
/// root, a path with no separator in it on any platform.
///
/// The rows failed because the fixture never reached the matcher. Its `entry.js` did a
/// bare `import 'testpkg'`, and `index.js` is not in the `sideEffects` list — so the
/// entry is side-effect-free, the whole import is legitimately dropped, and `fx.js` is
/// never even resolved. The expectation, not the bundler, was wrong.
///
/// Keeping the package entry alive makes the matcher run, and it is correct: `fx.js`
/// (matched, effectful) is retained and `pure.js` (unmatched, pure) is dropped.
fn write_live_side_effect_package(root: &Path, side_effects_field: &str) {
    write_side_effect_package(root, Some(side_effects_field));
    write_source(
        root,
        "node_modules/testpkg/index.js",
        "import './fx.js';\nimport './pure.js';\nexport const y = 2;",
    );
    write_source(root, "node_modules/testpkg/fx.js", "globalThis.__fx = 1;");
    write_source(
        root,
        "node_modules/testpkg/pure.js",
        "globalThis.__pure = 1;",
    );
    // Import a binding, so the entry cannot be tree-shaken away before the
    // sideEffects globs are ever consulted.
    write_source(root, "entry.js", "export { y } from 'testpkg';");
}

#[tokio::test]
async fn matrix_42_side_effects_string() {
    let root = temp_workspace();
    write_live_side_effect_package(&root, "\"./fx.js\"");

    let artifact = bundle_ok(&root, "entry.js", named(&["y"])).await;

    assert!(artifact.code.contains("__fx"), "{}", artifact.code);
    assert!(!artifact.code.contains("__pure"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 43: an array of sideEffects patterns behaves like row 42.
#[tokio::test]
async fn matrix_43_side_effects_array() {
    let root = temp_workspace();
    write_live_side_effect_package(&root, "[\"./fx.js\"]");

    let artifact = bundle_ok(&root, "entry.js", named(&["y"])).await;

    assert!(artifact.code.contains("__fx"), "{}", artifact.code);
    assert!(!artifact.code.contains("__pure"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 44: the NEAREST package manifest classifies each module — an
// importing package's sideEffects:true does not resurrect a transitive
// package that declares false. (The inverse — outer:false with an
// effectful inner — correctly prunes inner too, because skipping the
// declared-effect-free outer means its import of inner never executes;
// that is standard bundler subtree pruning, not a metadata leak.)
#[tokio::test]
async fn matrix_44_side_effects_nearest_transitive_package() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/outer/package.json",
        "{\"name\":\"outer\",\"version\":\"0.0.0\",\"main\":\"./index.js\",\"sideEffects\":true}",
    );
    write_source(
        &root,
        "node_modules/outer/index.js",
        "import 'inner';\nglobalThis.__outer = 1;",
    );
    write_source(
        &root,
        "node_modules/inner/package.json",
        "{\"name\":\"inner\",\"version\":\"0.0.0\",\"main\":\"./index.js\",\"sideEffects\":false}",
    );
    write_source(
        &root,
        "node_modules/inner/index.js",
        "globalThis.__inner = 1;",
    );
    write_source(&root, "entry.js", "import 'outer';\nexport const x = 1;");

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("__outer"), "{}", artifact.code);
    assert!(!artifact.code.contains("__inner"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Rows 45-48 extend the plan's original 44: independent review found four
// §10.2 items without a dedicated row.

// Row 45 (§10.2 "semantic failures"): a grammatical module that is
// semantically invalid — exporting an undeclared local binding — is a
// typed failure, not a partial bundle.
#[tokio::test]
async fn matrix_45_semantic_failure() {
    let root = temp_workspace();
    write_source(&root, "entry.js", "export { missing };");

    let failure = run(&root, "entry.js", named(&["missing"]))
        .await
        .expect_err("undeclared local export should fail");

    assert_eq!(failure.stage, "parse", "{failure:?}");
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 46 (§10.2 "namespace ... requests"): a Namespace selection is driven
// end-to-end, not just through the Full arm it currently shares.
#[tokio::test]
async fn matrix_46_namespace_selection() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "const d = 1;\nexport default d;\nexport const named = 2;",
    );

    let artifact = bundle_ok(&root, "entry.js", BundleSelection::Namespace).await;

    assert!(
        artifact
            .exported_names
            .contains(&"__il_entry_0_namespace".to_owned()),
        "exported names: {:?}",
        artifact.exported_names
    );
    assert!(artifact.code.contains("named"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 47 (§10.2 "non-exported effectful initializers"): an unexported
// declaration whose initializer has effects keeps the call and its
// dependency, exactly like the exported variant in row 19.
#[tokio::test]
async fn matrix_47_non_exported_effectful_initializer() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { compute } from './compute.js';\nimport { dep } from './dep.js';\n\
         const unused = compute(dep);\nexport const wanted = 1;",
    );
    write_source(
        &root,
        "compute.js",
        "export const compute = (value) => {\n  globalThis.__computed = value;\n  return value;\n};",
    );
    write_source(&root, "dep.js", "export const dep = 2;");

    let artifact = bundle_ok(&root, "entry.js", named(&["wanted"])).await;

    assert!(artifact.code.contains("__computed"), "{}", artifact.code);
    assert!(
        artifact
            .loaded_paths
            .iter()
            .any(|path| path.ends_with("dep.js")),
        "loaded paths: {:?}",
        artifact.loaded_paths
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 48 (§10.2 "symbol collisions" beyond the `__il_` case): two modules
// declaring the same top-level identifier deconflict; the semantic-validity
// gate inside bundle_ok is what proves no duplicate binding survived.
#[tokio::test]
async fn matrix_48_generic_symbol_collision() {
    let root = temp_workspace();
    write_source(
        &root,
        "entry.js",
        "import { left } from './a.js';\nimport { right } from './b.js';\nexport const x = left() + right();",
    );
    write_source(
        &root,
        "a.js",
        "const value = 1;\nexport const left = () => value;",
    );
    write_source(
        &root,
        "b.js",
        "const value = 2;\nexport const right = () => value;",
    );

    let artifact = bundle_ok(&root, "entry.js", named(&["x"])).await;

    assert!(artifact.code.contains("= 1"), "{}", artifact.code);
    assert!(artifact.code.contains("= 2"), "{}", artifact.code);
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

/// Installs a package under `node_modules` and returns the pair a production request is made
/// of: the entry file **inside** `node_modules`, and the package root beside it.
///
/// This is the shape [`write_side_effect_package`] cannot produce. That helper roots the build
/// at a workspace `entry.js` doing `import 'testpkg'`, which makes the package a **transitive**
/// module — resolved by Rolldown itself, and therefore handed its own `package.json`. Here the
/// package's entry **is** the `BundleEntry`, so our plugin resolves it, exactly as happens
/// whenever a user imports a package by name.
fn write_installed_package(
    root: &Path,
    name: &str,
    side_effects_field: Option<&str>,
    entry_source: &str,
) -> (PathBuf, PathBuf) {
    let side_effects = side_effects_field
        .map(|value| format!(",\"sideEffects\":{value}"))
        .unwrap_or_default();
    write_source(
        root,
        &format!("node_modules/{name}/package.json"),
        &format!(
            "{{\"name\":\"{name}\",\"version\":\"0.0.0\",\"type\":\"module\",\
             \"main\":\"./index.js\"{side_effects}}}"
        ),
    );
    write_source(root, &format!("node_modules/{name}/index.js"), entry_source);

    let package_root = root.join("node_modules").join(name);
    let entry_path = package_root.join("index.js");
    (entry_path, package_root)
}

// Row 49 (§7.4, ADR-0002): the PRODUCTION shape — the requested package's own entry file IS the
// `BundleEntry`, so our plugin resolves it rather than Rolldown.
//
// Every side-effects row above (38-44) roots its build at a workspace `entry.js` that does
// `import 'testpkg'`. That makes the package a TRANSITIVE module: Rolldown resolves it and hands
// the module its own `package.json`. Production is the opposite shape — the user imports
// `date-fns`, so `node_modules/date-fns/index.js` is the entry — and a plugin-resolved id carries
// only the metadata the plugin supplies, through `HookResolveIdOutput::package_json_path`. Leave
// that `None` and the entry module has no manifest at all: its side-effect classification falls
// back to pure source analysis, so a package declaring `sideEffects: false` keeps every top-level
// statement the analysis cannot prove pure — bytes Rollup and webpack drop, added to the number we
// report. So the rows that exist to prove "Rolldown owns `sideEffects`" all exercised the one code
// path production never takes. This is that path.
//
// `reported_side_effects` is deliberately `Unknown`: the statement must be dropped because
// Rolldown read the manifest we pointed it at, not because Import Lens told it anything (§7.4).
#[tokio::test]
async fn matrix_49_installed_package_entry_carries_its_manifest() {
    let root = temp_workspace();
    let (entry_path, package_root) = write_installed_package(
        &root,
        "purepkg",
        Some("false"),
        "export { parse } from './parse.js';\n\
         const defaults = { locale: \"en-US\" };\n\
         Object.freeze(defaults);\n",
    );
    write_source(
        &root,
        "node_modules/purepkg/parse.js",
        "export const parse = (input) => String(input).trim();",
    );

    let artifact = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![BundleEntry {
                entry_path,
                package_root,
                selection: named(&["parse"]),
                reported_side_effects: SideEffectsMode::Unknown,
            }],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
        .expect("bundle should succeed");
    assert_artifact_valid(&artifact);

    assert!(
        artifact.code.contains("trim"),
        "the requested export must survive:\n{}",
        artifact.code
    );
    assert!(
        !artifact.code.contains("Object.freeze"),
        "the package declares `sideEffects: false`, so its impure-looking top-level statement \
         must be dropped — retaining it means the entry module never learned which package it \
         belongs to:\n{}",
        artifact.code
    );
    assert!(
        !artifact.code.contains("en-US"),
        "the value the dropped statement reads must go with it:\n{}",
        artifact.code
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 50: the manifest carries a SECOND thing — the entry's module FORMAT.
//
// `infer_module_def_format` reads `"type"` from the same `package.json`, so a plugin-resolved
// `.js` entry is `ModuleDefFormat::Unknown` without it and `EsmPackageJson` with it. Rolldown
// gates Node's ESM-imports-CommonJS interop on exactly that
// (`should_consider_node_esm_spec_for_static_import` -> `def_format.is_esm()`): an ESM importer
// gets `__toESM(require_dep(), 1)`, whose `isNodeMode` flag makes the namespace's `default` the
// whole `module.exports` object, which is what Node does. An entry that does not know its own
// package's `type` silently gets the CommonJS-importer interop instead — a different `default`
// binding, and a different measured size (this is the one real-package number the manifest fix
// moved: react-loading-skeleton, +2 minified bytes, from the wrong interop to the right one).
//
// Row 49 cannot catch this: it would stay green for an implementation that supplied the manifest
// only for packages that declare `sideEffects`. This is the other half of the same field.
#[tokio::test]
async fn matrix_50_installed_esm_entry_gets_node_interop_for_commonjs() {
    let root = temp_workspace();
    let (entry_path, package_root) = write_installed_package(
        &root,
        "interoppkg",
        None,
        "import dep from './dep.cjs';\nexport const value = dep.value;\n",
    );
    write_source(
        &root,
        "node_modules/interoppkg/dep.cjs",
        "module.exports = { value: 1 };",
    );

    let artifact = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![BundleEntry {
                entry_path,
                package_root,
                selection: named(&["value"]),
                reported_side_effects: SideEffectsMode::Unknown,
            }],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
        .expect("bundle should succeed");
    assert_artifact_valid(&artifact);

    let interop = artifact
        .code
        .lines()
        .find(|line| line.contains("__toESM("))
        .unwrap_or_else(|| {
            panic!(
                "the CommonJS dependency must be imported through the interop helper:\n{}",
                artifact.code
            )
        });
    assert!(
        interop.contains(", 1)"),
        "the package declares `\"type\": \"module\"`, so its entry is an ES module and its static \
         CommonJS import must get Node's ESM interop (`__toESM(…, 1)`) — the bare form means the \
         entry never learned its own package's `type` and was finalized as a CommonJS \
         importer:\n{interop}"
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// Row 51: EACH entry's OWN package governs it — the multi-entry shape rows 49/50 cannot reach.
//
// Rows 49 and 50 are single-entry, so any entry->manifest mapping, right or wrong, points at the
// one manifest there is. The file-size path is the opposite: it submits every import of a file as
// entries of ONE build (`pipeline::file_size`), each `BundleEntry` carrying its own
// `package_root`. The plugin maps `import-lens:target/<i>` back to `targets[i]`, so the mapping is
// positional — and nothing else in the suite fails if that position slips.
//
// It is a worse bug than the one row 49 fixed, because it does not withhold metadata, it applies
// the WRONG package's declaration: swap the two manifests here and `dirtypkg`'s `Object.freeze` —
// a statement its `sideEffects: true` requires be kept — is silently deleted from the chunk, and
// its bytes vanish from the size we report. Measured: under that mutation every other matrix row,
// every badge row, every package row and the accuracy oracle stay green. This row is the only
// thing that goes red.
//
// Two entries, opposite declarations, one build: `purepkg` (`sideEffects: false`) must lose its
// freeze and `dirtypkg` (`sideEffects: true`) must keep its own.
#[tokio::test]
async fn matrix_51_multi_entry_manifests_do_not_cross() {
    let root = temp_workspace();
    let (pure_entry, pure_root) = write_installed_package(
        &root,
        "purepkg",
        Some("false"),
        "export { parse } from './parse.js';\n\
         const defaults = { locale: \"en-PURE\" };\n\
         Object.freeze(defaults);\n",
    );
    write_source(
        &root,
        "node_modules/purepkg/parse.js",
        "export const parse = (input) => String(input).trim();",
    );
    let (dirty_entry, dirty_root) = write_installed_package(
        &root,
        "dirtypkg",
        Some("true"),
        "export { format } from './format.js';\n\
         const settings = { locale: \"en-DIRTY\" };\n\
         Object.freeze(settings);\n",
    );
    write_source(
        &root,
        "node_modules/dirtypkg/format.js",
        "export const format = (input) => String(input).toUpperCase();",
    );

    let artifact = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![
                BundleEntry {
                    entry_path: pure_entry,
                    package_root: pure_root,
                    selection: named(&["parse"]),
                    reported_side_effects: SideEffectsMode::Unknown,
                },
                BundleEntry {
                    entry_path: dirty_entry,
                    package_root: dirty_root,
                    selection: named(&["format"]),
                    reported_side_effects: SideEffectsMode::Unknown,
                },
            ],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::FileSize,
        })
        .await
        .expect("bundle should succeed");
    assert_artifact_valid(&artifact);

    assert!(
        artifact.code.contains("trim") && artifact.code.contains("toUpperCase"),
        "both requested exports must survive:\n{}",
        artifact.code
    );
    assert!(
        !artifact.code.contains("en-PURE"),
        "`purepkg` declares `sideEffects: false`, so ITS impure-looking top-level statement must be \
         dropped — retaining it means entry 0 was governed by some other package's manifest:\n{}",
        artifact.code
    );
    assert!(
        artifact.code.contains("en-DIRTY"),
        "`dirtypkg` declares `sideEffects: true`, so ITS top-level statement must be KEPT — \
         dropping it means entry 1 was governed by `purepkg`'s manifest, and bytes that really \
         ship have vanished from the size we report:\n{}",
        artifact.code
    );
    assert_eq!(
        artifact.code.matches("Object.freeze").count(),
        1,
        "exactly one of the two freezes — `dirtypkg`'s — may survive:\n{}",
        artifact.code
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}
