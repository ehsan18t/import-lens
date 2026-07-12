//! Combined file sizing must size each import under its OWN runtime.
//!
//! `compute_file_size` hands every entry to one `BundleRequest`, which carries a
//! single runtime. Root entries are pre-resolved per request, so their own paths
//! are right either way — but Rolldown resolves the whole TRANSITIVE graph under
//! that one runtime, and Server and Client resolve dependencies under materially
//! different conditions. Applying the first resolved import's runtime to every
//! entry therefore resolves the other runtime's dependencies against the wrong
//! conditions, and the mis-conditioned build still SUCCEEDS — so no fallback fires
//! and no diagnostic is raised. The user just gets a wrong number.
//!
//! A single Astro file reaches this: frontmatter imports are Server, processed
//! `<script>` imports are Client (`document/script_regions.rs`).
//!
//! Spec: I15.

use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::analyze::AnalysisContext;
use import_lens_daemon::pipeline::file_size::{
    FileSizeComputation, SizedImport, compute_file_size,
};
use std::{fs, path::Path, path::PathBuf};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-file-size-runtime")
}

fn context(workspace: &Path) -> AnalysisContext {
    AnalysisContext {
        workspace_root: workspace.to_path_buf(),
        active_document_path: workspace.join("src").join("page.astro"),
    }
}

fn request(package: &str, export: &str, runtime: ImportRuntime) -> SizedImport {
    SizedImport {
        request: ImportRequest {
            specifier: package.to_owned(),
            package_name: package.to_owned(),
            version: "1.0.0".to_owned(),
            named: vec![export.to_owned()],
            import_kind: ImportKind::Named,
            runtime,
        },
        // Nothing measured yet: these sizings exercise the combined build, which is what the
        // file's totals come from — the per-import measurements only feed the fallback.
        result: None,
    }
}

fn size(context: &AnalysisContext, requests: &[SizedImport]) -> FileSizeComputation {
    let computed = compute_file_size(context, requests);
    assert_eq!(
        computed.error, None,
        "file sizing should succeed: {:?}",
        computed.diagnostics
    );
    computed
}

/// `host` has one runtime-independent entry that re-exports `cond`, whose own
/// `exports` map is runtime-conditional: a single const under `browser`, a large
/// module under `node`. The conditional resolution therefore happens where
/// Rolldown owns it — transitively — which is the only place the defect lives.
///
/// `plain` shares no modules with either, so it can be sized under a different
/// runtime without any shared-module interaction confusing the comparison.
fn write_packages(workspace: &Path) {
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

    let plain = workspace.join("node_modules").join("plain");
    fs::create_dir_all(&plain).expect("plain package root");
    fs::write(
        plain.join("package.json"),
        r#"{"name":"plain","version":"1.0.0","type":"module","sideEffects":false,"module":"./index.js"}"#,
    )
    .expect("plain manifest");
    fs::write(plain.join("index.js"), "export const thing = 2;\n").expect("plain entry");
}

#[test]
fn combined_file_size_does_not_depend_on_import_order() {
    let workspace = temp_workspace();
    write_packages(&workspace);
    let context = context(&workspace);

    let host_client = size(&context, &[request("host", "value", ImportRuntime::Client)]);
    let host_server = size(&context, &[request("host", "value", ImportRuntime::Server)]);

    // If the two runtimes do not resolve `cond` differently, this test proves
    // nothing. Fail loudly rather than pass vacuously.
    assert!(
        host_server.raw_bytes > host_client.raw_bytes * 2,
        "fixture is broken: `host` must resolve differently per runtime \
         (server={} client={})",
        host_server.raw_bytes,
        host_client.raw_bytes,
    );

    // The same two imports, both orders. The entry count and the virtual-entry
    // facade are identical between them, so the ONLY difference is which import
    // resolves first — and therefore which runtime the single build would apply to
    // every entry. Order-invariance isolates the defect exactly.
    let client_first = size(
        &context,
        &[
            request("host", "value", ImportRuntime::Client),
            request("plain", "thing", ImportRuntime::Server),
        ],
    );
    let server_first = size(
        &context,
        &[
            request("plain", "thing", ImportRuntime::Server),
            request("host", "value", ImportRuntime::Client),
        ],
    );

    assert_eq!(
        client_first.raw_bytes, server_first.raw_bytes,
        "combined file size must not depend on import order: `host` is a Client import \
         and must be sized under Client conditions no matter which import resolved \
         first. client-first={}, server-first={} (spec I15).",
        client_first.raw_bytes, server_first.raw_bytes,
    );

    // ...and the order-invariant answer must be the CLIENT-conditioned one. Guards
    // against a "fix" that makes both orders agree by sizing everything as Server.
    assert!(
        server_first.raw_bytes < host_server.raw_bytes,
        "`host` is a Client import and must not be sized under Server conditions: \
         aggregate={} is at or above the Server-only size {}",
        server_first.raw_bytes,
        host_server.raw_bytes,
    );
}

/// A package imported under two different runtimes in one file is a real shape
/// (an Astro island imported in frontmatter and in a `<script>`). Each runtime
/// genuinely ships its own copy, so it is counted once per runtime.
#[test]
fn a_package_imported_under_two_runtimes_is_counted_once_per_runtime() {
    let workspace = temp_workspace();
    write_packages(&workspace);
    let context = context(&workspace);

    let client_only = size(
        &context,
        &[request("plain", "thing", ImportRuntime::Client)],
    );
    let both = size(
        &context,
        &[
            request("plain", "thing", ImportRuntime::Client),
            request("plain", "thing", ImportRuntime::Server),
        ],
    );

    assert!(
        both.raw_bytes > client_only.raw_bytes,
        "the same package imported under two runtimes ships two copies and must be \
         counted twice: both={} client_only={}",
        both.raw_bytes,
        client_only.raw_bytes,
    );
}
