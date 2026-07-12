//! Product-level execution-boundary coverage (spec §9).

use import_lens_daemon::{
    engine::{BundleEntry, BundlePurpose, BundleRequest, BundleSelection, EngineBudget, boundary},
    ipc::protocol::ImportRuntime,
    pipeline::resolver::SideEffectsMode,
};
use std::{fs, path::Path};

mod common;

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

#[test]
fn boundary_caps_concurrent_builds_at_two_permits() {
    let root = common::temp_workspace("import-lens-engine-boundary");
    let package_root = root.join("node_modules/boundary-pkg");
    write_source(
        &root,
        "node_modules/boundary-pkg/package.json",
        r#"{"name":"boundary-pkg","version":"1.0.0","module":"./index.js","sideEffects":false}"#,
    );
    write_source(
        &root,
        "node_modules/boundary-pkg/index.js",
        "export const alpha = 1; export const beta = 2; export const gamma = 3;",
    );
    let entry = package_root.join("index.js");

    let results = std::thread::scope(|scope| {
        let handles = ["alpha", "beta", "gamma"]
            .into_iter()
            .map(|name| {
                let entry_path = entry.clone();
                let package_root = package_root.clone();
                scope.spawn(move || {
                    boundary::bundle_sync(
                        BundleRequest {
                            entries: vec![BundleEntry {
                                entry_path,
                                package_root,
                                selection: BundleSelection::Named(vec![name.to_owned()]),
                                reported_side_effects: SideEffectsMode::False,
                            }],
                            runtime: ImportRuntime::Component,
                            purpose: BundlePurpose::ImportSize,
                        },
                        EngineBudget::interactive(),
                    )
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("boundary caller should not panic"))
            .collect::<Vec<_>>()
    });

    for result in results {
        assert!(
            !result
                .expect("boundary bundle should succeed")
                .code
                .is_empty()
        );
    }
    assert!((1..=2).contains(&boundary::peak_in_flight()));
    fs::remove_dir_all(root).expect("temp workspace should be removed");
}
