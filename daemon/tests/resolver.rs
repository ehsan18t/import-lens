use import_lens_daemon::{
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::resolver::resolve_package_entry,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-resolver")
}

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn request(package_name: &str, runtime: ImportRuntime) -> ImportRequest {
    request_for_specifier(package_name, package_name, runtime)
}

fn request_for_specifier(
    specifier: &str,
    package_name: &str,
    runtime: ImportRuntime,
) -> ImportRequest {
    ImportRequest {
        specifier: specifier.to_owned(),
        package_name: package_name.to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime,
    }
}

#[test]
fn resolver_uses_browser_main_field_for_component_runtime_without_exports() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/profiled-pkg/package.json",
        r#"{"version":"1.0.0","browser":"browser.js","module":"module.js","main":"main.cjs"}"#,
    );
    write_source(
        &root,
        "node_modules/profiled-pkg/browser.js",
        "export const target = 'browser';",
    );
    write_source(
        &root,
        "node_modules/profiled-pkg/module.js",
        "export const target = 'module';",
    );
    write_source(
        &root,
        "node_modules/profiled-pkg/main.cjs",
        "exports.target = 'main';",
    );

    let resolved = resolve_package_entry(
        &root.join("src").join("app.ts"),
        &request("profiled-pkg", ImportRuntime::Component),
    )
    .expect("component runtime should resolve package entry");

    fs::remove_dir_all(root).expect("temp resolver workspace should be removed");
    assert!(resolved.entry_path.ends_with("browser.js"), "{resolved:?}");
    assert!(!resolved.is_cjs, "{resolved:?}");
}

#[test]
fn resolver_uses_node_condition_for_server_runtime_exports() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/conditional-pkg/package.json",
        r#"{"version":"1.0.0","exports":{"browser":"./browser.js","node":"./node.js","default":"./default.js"}}"#,
    );
    write_source(
        &root,
        "node_modules/conditional-pkg/browser.js",
        "export const target = 'browser';",
    );
    write_source(
        &root,
        "node_modules/conditional-pkg/node.js",
        "export const target = 'node';",
    );
    write_source(
        &root,
        "node_modules/conditional-pkg/default.js",
        "export const target = 'default';",
    );

    let resolved = resolve_package_entry(
        &root.join("src").join("page.ts"),
        &request("conditional-pkg", ImportRuntime::Server),
    )
    .expect("server runtime should resolve node condition");

    fs::remove_dir_all(root).expect("temp resolver workspace should be removed");
    assert!(resolved.entry_path.ends_with("node.js"), "{resolved:?}");
}

#[test]
fn resolver_keeps_import_condition_file_with_require_string_as_esm() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/string-import-pkg/package.json",
        r#"{"version":"1.0.0","exports":{"import":"./index.js","default":"./index.cjs"}}"#,
    );
    write_source(
        &root,
        "node_modules/string-import-pkg/index.js",
        r#"export const text = "literal require("; "#,
    );
    write_source(
        &root,
        "node_modules/string-import-pkg/index.cjs",
        "exports.text = 'commonjs';",
    );

    let resolved = resolve_package_entry(
        &root.join("src").join("app.ts"),
        &request("string-import-pkg", ImportRuntime::Component),
    )
    .expect("import condition entry should resolve");

    fs::remove_dir_all(root).expect("temp resolver workspace should be removed");
    assert!(resolved.entry_path.ends_with("index.js"), "{resolved:?}");
    assert!(!resolved.is_cjs, "{resolved:?}");
}

#[test]
fn resolver_marks_commonjs_type_js_subpath_as_cjs() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/typed-cjs-pkg/package.json",
        r#"{"version":"1.0.0","type":"commonjs"}"#,
    );
    write_source(
        &root,
        "node_modules/typed-cjs-pkg/subpath.js",
        "const value = 1;",
    );

    let resolved = resolve_package_entry(
        &root.join("src").join("app.ts"),
        &request_for_specifier(
            "typed-cjs-pkg/subpath",
            "typed-cjs-pkg",
            ImportRuntime::Component,
        ),
    )
    .expect("CommonJS typed subpath should resolve");

    fs::remove_dir_all(root).expect("temp resolver workspace should be removed");
    assert!(resolved.entry_path.ends_with("subpath.js"), "{resolved:?}");
    assert!(resolved.is_cjs, "{resolved:?}");
}

#[test]
fn resolver_does_not_validate_root_entry_fields_for_resolved_subpath_imports() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/subpath-pkg/package.json",
        r#"{"version":"1.0.0","module":"missing-root-entry.js"}"#,
    );
    write_source(
        &root,
        "node_modules/subpath-pkg/subpath.js",
        "export const target = 'subpath';",
    );

    let resolved = resolve_package_entry(
        &root.join("src").join("app.ts"),
        &request_for_specifier(
            "subpath-pkg/subpath",
            "subpath-pkg",
            ImportRuntime::Component,
        ),
    )
    .expect("valid subpath should not be rejected by broken root entry fields");

    fs::remove_dir_all(root).expect("temp resolver workspace should be removed");
    assert!(resolved.entry_path.ends_with("subpath.js"), "{resolved:?}");
}

#[test]
fn resolver_keeps_mjs_and_module_type_entries_as_esm() {
    let root = temp_workspace();
    write_source(
        &root,
        "node_modules/mjs-pkg/package.json",
        r#"{"version":"1.0.0","main":"index.mjs"}"#,
    );
    write_source(
        &root,
        "node_modules/mjs-pkg/index.mjs",
        r#"export const text = "module.exports";"#,
    );
    write_source(
        &root,
        "node_modules/module-type-pkg/package.json",
        r#"{"version":"1.0.0","type":"module","main":"index.js"}"#,
    );
    write_source(
        &root,
        "node_modules/module-type-pkg/index.js",
        r#"export const text = "require("; "#,
    );

    let mjs = resolve_package_entry(
        &root.join("src").join("app.ts"),
        &request("mjs-pkg", ImportRuntime::Component),
    )
    .expect(".mjs entry should resolve");
    let module_type = resolve_package_entry(
        &root.join("src").join("app.ts"),
        &request("module-type-pkg", ImportRuntime::Component),
    )
    .expect("module type entry should resolve");

    fs::remove_dir_all(root).expect("temp resolver workspace should be removed");
    assert!(!mjs.is_cjs, "{mjs:?}");
    assert!(!module_type.is_cjs, "{module_type:?}");
}

#[test]
fn shared_resolver_reflects_node_modules_change_only_after_invalidation() {
    let root = temp_workspace();
    write_source(&root, "src/app.ts", "");
    write_source(
        &root,
        "node_modules/swap-lib/package.json",
        r#"{"version":"1.0.0","module":"a.js"}"#,
    );
    write_source(&root, "node_modules/swap-lib/a.js", "export const value = 'a';");
    write_source(&root, "node_modules/swap-lib/b.js", "export const value = 'b';");
    let document = root.join("src").join("app.ts");

    let first = resolve_package_entry(&document, &request("swap-lib", ImportRuntime::Component))
        .expect("first resolve")
        .entry_path;
    assert!(first.ends_with("a.js"), "{first:?}");

    write_source(
        &root,
        "node_modules/swap-lib/package.json",
        r#"{"version":"1.0.0","module":"b.js"}"#,
    );
    let stale = resolve_package_entry(&document, &request("swap-lib", ImportRuntime::Component))
        .expect("stale resolve")
        .entry_path;
    assert!(
        stale.ends_with("a.js"),
        "shared resolver cache should persist until invalidated: {stale:?}"
    );

    import_lens_daemon::pipeline::resolver::invalidate_shared_resolvers();
    let fresh = resolve_package_entry(&document, &request("swap-lib", ImportRuntime::Component))
        .expect("fresh resolve")
        .entry_path;

    fs::remove_dir_all(root).expect("cleanup");
    assert!(
        fresh.ends_with("b.js"),
        "resolution should update after invalidation: {fresh:?}"
    );
}

#[test]
fn find_package_root_error_lists_probed_paths() {
    let root = temp_workspace();
    write_source(&root, "src/app.ts", "");
    let document = root.join("src").join("app.ts");

    let error = import_lens_daemon::pipeline::resolver::find_package_root(&document, "nope-lib")
        .expect_err("missing package should error");

    fs::remove_dir_all(root).expect("cleanup");
    assert!(error.contains("nope-lib"), "{error}");
    assert!(error.contains("checked:"), "error should list probed paths: {error}");
}
