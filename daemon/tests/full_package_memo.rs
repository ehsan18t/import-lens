//! The full-package comparison build is memoized per (entry, runtime).
//!
//! `truly_treeshakeable` compares the named import against the whole package, and
//! answering that costs a second complete Rolldown build plus a second complete
//! minify — of which only the minified *length* is used. That length does not
//! depend on which names were imported, but the import cache key does, so every
//! named variant of one entry used to pay for its own copy: 2N builds for N
//! variants of the same package.
//!
//! This measures the engine's own build counter, which is the honest unit — a
//! build is the most expensive thing the daemon does. Both the counter and the
//! memo are process-global, so this test owns its binary: a neighbouring test
//! bundling anything at all would corrupt the deltas.

use import_lens_daemon::engine::boundary::builds_started;
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::analyze::{AnalysisContext, analyze_import};
use std::sync::Mutex;
use std::{fs, path::Path, path::PathBuf};

mod common;

/// `sideEffects: false` is what gates the comparison build — this is the popular
/// tree-shakeable set (lodash-es, date-fns, zod), not a corner case.
fn write_package(workspace: &Path) {
    let root = workspace.join("node_modules").join("pkg");
    fs::create_dir_all(&root).expect("package root should be created");
    fs::write(
        root.join("package.json"),
        r#"{"name":"pkg","version":"1.0.0","type":"module","sideEffects":false,"module":"./index.js"}"#,
    )
    .expect("manifest should be written");
    fs::write(
        root.join("index.js"),
        "export { alpha } from './alpha.js';\nexport { beta } from './beta.js';\n",
    )
    .expect("entry should be written");
    fs::write(root.join("beta.js"), "export const beta = () => 'beta';\n")
        .expect("beta should be written");
    write_alpha(workspace, "alpha");
}

fn write_alpha(workspace: &Path, body: &str) {
    let path = workspace.join("node_modules").join("pkg").join("alpha.js");
    fs::write(path, format!("export const alpha = () => '{body}';\n"))
        .expect("alpha should be written");
}

/// The build counter and the memo are process-global, and cargo runs the tests in
/// one binary on parallel threads. Each test must own the process while it measures.
static SERIAL: Mutex<()> = Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn analyze(workspace: &Path, name: &str) -> usize {
    let before = builds_started();
    let result = analyze_import(
        &AnalysisContext {
            workspace_root: workspace.to_path_buf(),
            active_document_path: workspace.join("src").join("app.ts"),
        },
        &ImportRequest {
            specifier: "pkg".to_owned(),
            package_name: "pkg".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec![name.to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        },
    );
    assert!(
        result.error.is_none(),
        "pkg/{name} should analyze: {result:?}"
    );
    assert!(
        result.truly_treeshakeable,
        "pkg/{name} imports one of two independent exports and must read as tree-shakeable — \
         if this is false the comparison length is wrong, not merely expensive: {result:?}"
    );
    builds_started() - before
}

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-full-package-memo")
}

#[test]
fn the_comparison_build_is_paid_once_per_entry_and_expires_on_edit() {
    let _serial = serialized();
    let workspace = temp_workspace();
    write_package(&workspace);

    // First named import: its own build, plus the full-package comparison.
    assert_eq!(
        analyze(&workspace, "alpha"),
        2,
        "a cold named import pays for its own build and the full-package comparison"
    );

    // A different name off the same entry. The comparison answer is identical, so
    // only the import's own build should run. Before the memo this was 2 again.
    assert_eq!(
        analyze(&workspace, "beta"),
        1,
        "a second named import off the same entry must reuse the memoized comparison"
    );

    // Editing a module the comparison measured must expire it. Same length and a
    // fresh mtime is the easy case; the memo validates with the same strict
    // hash-verifying check the import cache uses.
    write_alpha(&workspace, "gamma");
    assert_eq!(
        analyze(&workspace, "alpha"),
        2,
        "editing a module the comparison measured must expire the memo"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}

/// Fingerprints alone cannot make this memo safe, and a cache that only *usually*
/// expires is worse than no cache: it puts a wrong number on screen.
///
/// `first_party_manifests` deliberately skips everything under `node_modules`,
/// because an installed manifest cannot change without an install — and an install
/// bumps the cache generation, which is the backstop the import cache leans on. A
/// memo that ignored the generation would not get that backstop: `pnpm install`
/// can repoint a dependency's `exports` at a different file while leaving its
/// sources byte-identical, and every fingerprint the memo holds would still hash
/// clean over a length measured against the *old* resolution. The same bump is
/// what `invalidate_all` — the user's "clear the cache" escape hatch — relies on.
///
/// So: bump the generation and require the memo to rebuild, with no file touched.
#[test]
fn a_cache_generation_bump_expires_the_memo() {
    let _serial = serialized();
    let workspace = temp_workspace();
    write_package(&workspace);

    assert_eq!(analyze(&workspace, "alpha"), 2, "cold: entry + comparison");
    assert_eq!(analyze(&workspace, "beta"), 1, "warm: comparison memoized");

    // Exactly what invalidate_package / invalidate_all / node_modules_changed do.
    import_lens_daemon::cache::memory::bump_cache_generation();

    assert_eq!(
        analyze(&workspace, "beta"),
        2,
        "a cache-generation bump must expire the memo even though no fingerprinted \
         file changed — this is the only thing standing between a reinstall and a \
         stale full-package length"
    );

    fs::remove_dir_all(workspace).expect("temp workspace should be removed");
}
