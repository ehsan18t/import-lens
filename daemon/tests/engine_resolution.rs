//! The engine must resolve transitive dependencies under the conditions the
//! requested runtime asks for — and no others.
//!
//! Rolldown derives an unset `platform` from the output format, and `Esm` derives
//! `Platform::Browser`. `rolldown_resolver::ResolverConfig::build` then *appends*
//! a platform condition to whatever condition list it was given:
//!
//! ```text
//! Platform::Node    => conditions.push("node")
//! Platform::Browser => conditions.push("browser")
//! Platform::Neutral => {}
//! ```
//!
//! So with the platform left unset, the daemon's Server runtime — whose conditions
//! are `[node, server, module, import, default]` — silently also matched `browser`,
//! and a package whose `exports` map lists `browser` first resolved its *browser*
//! build under Server. Export conditions are matched in the package's own key
//! order, so the extra condition wins on exactly the packages that care.
//!
//! `Platform::Neutral` appends nothing, leaving the per-runtime condition list from
//! the shared resolver authoritative (spec §7.1).

use import_lens_daemon::engine::EngineBudget;
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::analyze::AnalysisContext;
use import_lens_daemon::pipeline::file_size::compute_file_size;
use std::{fs, path::Path, path::PathBuf};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-engine-resolution")
}

fn context(workspace: &Path) -> AnalysisContext {
    AnalysisContext {
        workspace_root: workspace.to_path_buf(),
        active_document_path: workspace.join("src").join("page.astro"),
        engine_budget: EngineBudget::interactive(),
    }
}

fn request(package: &str, export: &str, runtime: ImportRuntime) -> ImportRequest {
    ImportRequest {
        specifier: package.to_owned(),
        package_name: package.to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![export.to_owned()],
        import_kind: ImportKind::Named,
        runtime,
    }
}

/// `host` has a single, runtime-independent entry: the root entry is pre-resolved
/// per request, so a runtime-conditional *entry* would be resolved correctly anyway
/// and would prove nothing. The conditional resolution has to happen one level down,
/// where Rolldown owns it — which is exactly where the platform condition leaked in.
///
/// `host` therefore re-exports `cond`, whose own `exports` map lists `browser`
/// before `node`. Under Server conditions the `node` build must win; if `browser`
/// is also active, the browser build wins instead because it is listed first.
fn write_conditional_packages(workspace: &Path) {
    let host = workspace.join("node_modules").join("host");
    fs::create_dir_all(&host).expect("host package root");
    fs::write(
        host.join("package.json"),
        r#"{"name":"host","version":"1.0.0","type":"module","sideEffects":false,"module":"./index.js"}"#,
    )
    .expect("host manifest");
    fs::write(host.join("index.js"), "export { value } from \"cond\";\n").expect("host entry");

    let cond = workspace.join("node_modules").join("cond");
    fs::create_dir_all(&cond).expect("cond package root");
    fs::write(
        cond.join("package.json"),
        r#"{
  "name": "cond",
  "version": "1.0.0",
  "type": "module",
  "sideEffects": false,
  "main": "./server.js",
  "browser": "./browser.js",
  "exports": {
    ".": {
      "browser": "./browser.js",
      "node": "./server.js",
      "default": "./server.js"
    }
  }
}"#,
    )
    .expect("cond manifest");

    // The browser build is a single const; the node build drags in a large module.
    // The size gap is what makes a mis-resolved condition visible in bytes.
    fs::write(cond.join("browser.js"), "export const value = 1;\n").expect("cond browser entry");
    fs::write(
        cond.join("server.js"),
        "export { value } from \"./heavy.js\";\n",
    )
    .expect("cond server entry");

    let mut heavy = String::from("export const value = [\n");
    for index in 0..600 {
        heavy.push_str(&format!("  \"heavy padding entry number {index:04}\",\n"));
    }
    heavy.push_str("];\n");
    fs::write(cond.join("heavy.js"), heavy).expect("cond heavy module");
}

#[test]
fn server_runtime_resolves_the_node_export_condition_not_the_browser_one() {
    let workspace = temp_workspace();
    write_conditional_packages(&workspace);
    let context = context(&workspace);

    let client = compute_file_size(&context, &[request("host", "value", ImportRuntime::Client)]);
    let server = compute_file_size(&context, &[request("host", "value", ImportRuntime::Server)]);

    assert_eq!(client.error, None, "client sizing should succeed");
    assert_eq!(server.error, None, "server sizing should succeed");

    // The node build re-exports heavy.js (~20 KB); the browser build is one const.
    // If Server also matched `browser`, `cond`'s browser entry would win — it is
    // listed first in the exports map — and Server would report the tiny size.
    assert!(
        server.raw_bytes > client.raw_bytes * 2,
        "a Server-runtime import must resolve `cond`'s `node` export condition, not its \
         `browser` one. server={} client={} — an unset Rolldown `platform` derives \
         Platform::Browser from `format: Esm` and appends `browser` to the Server \
         condition list, so the browser build wins on any package that lists it first.",
        server.raw_bytes,
        client.raw_bytes,
    );
}
