use import_lens_daemon::{
    ipc::protocol::{ImportKind, ImportRequest, ImportRuntime},
    pipeline::resolver::resolve_package_entry,
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
    let path = std::env::temp_dir().join(format!("import-lens-resolver-{suffix}"));
    fs::create_dir_all(&path).expect("temp resolver workspace should be created");
    path
}

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn request(package_name: &str, runtime: ImportRuntime) -> ImportRequest {
    ImportRequest {
        specifier: package_name.to_owned(),
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
