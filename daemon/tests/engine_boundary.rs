//! Phase 2 integration tests (spec §9, §11): the execution boundary caps
//! concurrent builds, and the wired engine paths produce sane results next
//! to the legacy pipeline while `USE_ROLLDOWN_ENGINE` keeps production on
//! the old engine. Values are expected to differ between engines; the
//! assertions are qualitative.

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, BundleSelection, boundary,
};
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportRuntime};
use import_lens_daemon::pipeline::analyze::{
    AnalysisContext, analyze_resolved_import, analyze_with_rolldown_engine,
};
use import_lens_daemon::pipeline::file_size::compute_file_size_with_engine;
use import_lens_daemon::pipeline::resolver::{SideEffectsMode, resolve_package_entry};
use std::fs;
use std::path::Path;

mod common;

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn write_test_package(root: &Path) {
    write_source(
        root,
        "node_modules/boundary-pkg/package.json",
        "{\"name\":\"boundary-pkg\",\"version\":\"1.0.0\",\"module\":\"./index.js\",\"sideEffects\":false}",
    );
    write_source(
        root,
        "node_modules/boundary-pkg/index.js",
        "export { alpha } from './alpha.js';\nexport { beta } from './beta.js';\nexport { gamma } from './gamma.js';",
    );
    write_source(
        root,
        "node_modules/boundary-pkg/alpha.js",
        "import { shared } from './shared.js';\nexport const alpha = () => shared(1);",
    );
    write_source(
        root,
        "node_modules/boundary-pkg/beta.js",
        "import { shared } from './shared.js';\nexport const beta = () => shared(2);",
    );
    write_source(
        root,
        "node_modules/boundary-pkg/gamma.js",
        // The §2.2 escaping-namespace-over-empty-module construct the legacy
        // engine emits dangling references for.
        "import * as ns from './empty.js';\nexport const gamma = () => ns;",
    );
    write_source(root, "node_modules/boundary-pkg/empty.js", "");
    write_source(
        root,
        "node_modules/boundary-pkg/shared.js",
        // Big enough that double-counting it dominates any per-build glue,
        // so the combined-vs-sum assertion below is robust.
        "const table = ['zero', 'one', 'two', 'three', 'four', 'five', 'six', 'seven'];\n\
         const describe = (value) => `${value} is ${table[value] ?? 'many'}`;\n\
         export const shared = (value) => {\n  const label = describe(value);\n  return `${label}!`;\n};",
    );
    write_source(root, "src/app.ts", "");
}

fn import_request(named: &str) -> ImportRequest {
    ImportRequest {
        specifier: "boundary-pkg".to_owned(),
        package_name: "boundary-pkg".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec![named.to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    }
}

// Spec §9: at most two builds in flight daemon-wide, callable from plain
// (non-Tokio) threads, all callers completing.
#[test]
fn boundary_caps_concurrent_builds_at_two_permits() {
    let root = common::temp_workspace("import-lens-engine-boundary");
    write_test_package(&root);
    let entry = root.join("node_modules/boundary-pkg/index.js");
    let package_root = root.join("node_modules/boundary-pkg");

    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..3)
            .map(|index| {
                let entry = entry.clone();
                let package_root = package_root.clone();
                let named = ["alpha", "beta", "gamma"][index].to_owned();
                scope.spawn(move || {
                    boundary::bundle_sync(BundleRequest {
                        entries: vec![BundleEntry {
                            entry_path: entry,
                            package_root,
                            selection: BundleSelection::Named(vec![named]),
                            reported_side_effects: SideEffectsMode::False,
                        }],
                        runtime: ImportRuntime::Component,
                        purpose: BundlePurpose::ImportSize,
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("boundary caller should not panic"))
            .collect()
    });

    for result in results {
        let artifact = result.expect("boundary bundle should succeed");
        assert!(!artifact.code.is_empty());
    }
    let peak = boundary::peak_in_flight();
    assert!(
        (1..=2).contains(&peak),
        "peak in-flight builds should be capped at the two permits, got {peak}"
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// The wired individual-analysis path produces a sane result next to the
// legacy pipeline — including on the §2.2 construct where the legacy output
// is defective. Sizes are expected to differ; the comparison is qualitative.
#[test]
fn engine_analysis_path_is_sane_next_to_legacy() {
    let root = common::temp_workspace("import-lens-engine-diff");
    write_test_package(&root);
    let context = AnalysisContext {
        workspace_root: root.clone(),
        active_document_path: root.join("src/app.ts"),
    };

    for named in ["alpha", "gamma"] {
        let request = import_request(named);
        let resolved = resolve_package_entry(&context.active_document_path, &request)
            .expect("test package should resolve");

        let legacy = analyze_resolved_import(&context, &request, resolved.clone());
        let (engine, loaded_paths) = analyze_with_rolldown_engine(
            &context,
            &request,
            &resolved.entry_path,
            &resolved.package_root,
            &resolved.side_effects,
            resolved.is_cjs,
        )
        .expect("engine analysis should succeed");

        assert_eq!(legacy.error, None, "{named}: {legacy:?}");
        assert_eq!(engine.error, None, "{named}: {engine:?}");
        assert!(
            engine.raw_bytes > 0 && engine.minified_bytes > 0,
            "{engine:?}"
        );
        assert_eq!(engine.side_effects, legacy.side_effects, "{named}");
        assert_eq!(engine.is_cjs, legacy.is_cjs, "{named}");
        // §8.3: fingerprints cover the manifest and every loaded module.
        assert!(
            loaded_paths
                .iter()
                .any(|path| path.ends_with("package.json")),
            "loaded paths must include the manifest: {loaded_paths:?}"
        );
        assert!(
            loaded_paths.iter().any(|path| path.ends_with("index.js")),
            "loaded paths must include the entry: {loaded_paths:?}"
        );
    }
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}

// The combined file-size path issues ONE multi-entry build (§6.3) and
// produces a sane computation for imports sharing a transitive dependency.
#[test]
fn engine_file_size_path_combines_imports_in_one_build() {
    let root = common::temp_workspace("import-lens-engine-filesize");
    write_test_package(&root);
    let context = AnalysisContext {
        workspace_root: root.clone(),
        active_document_path: root.join("src/app.ts"),
    };

    let requests = vec![import_request("alpha"), import_request("beta")];
    let combined = compute_file_size_with_engine(&context, &requests);
    let alpha_alone = compute_file_size_with_engine(&context, &requests[..1]);
    let beta_alone = compute_file_size_with_engine(&context, &requests[1..]);

    assert_eq!(combined.error, None, "{combined:?}");
    assert!(
        combined.raw_bytes > 0 && combined.minified_bytes > 0,
        "{combined:?}"
    );
    // The dedup proof: separate builds each carry the shared dependency, so
    // one combined multi-entry build must come in under their sum.
    assert!(
        combined.raw_bytes < alpha_alone.raw_bytes + beta_alone.raw_bytes,
        "combined {} should be smaller than {} + {}",
        combined.raw_bytes,
        alpha_alone.raw_bytes,
        beta_alone.raw_bytes
    );
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}
