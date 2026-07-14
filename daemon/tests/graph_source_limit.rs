//! The total-source ceiling (`MAX_GRAPH_SOURCE_BYTES`) is the one graph limit
//! whose breach cannot be provoked cheaply: at the shipped 100 MiB default it
//! needs >100 MiB of fixtures, so the matrix row covering it was `#[ignore]`d
//! and the branch never actually ran (spec §10.4 requires every graph limit to
//! be observable).
//!
//! `#[cfg(test)]` cannot shrink the ceiling — integration tests link the daemon
//! library compiled *without* `cfg(test)` — so `limits.rs` reads it from the
//! environment instead. The override is process-wide and cached on first use, so
//! this row lives in its own test file: cargo gives each integration test file
//! its own process, which makes the `set_var` below deterministic and keeps the
//! shrunk ceiling from leaking into any other row.
//!
//! This is a distinct branch from the per-module cap (matrix row 33): each module
//! here stays far under `MAX_MODULE_SOURCE_BYTES`, so only the accumulator can trip.

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, BundleSelection, ImportRuntime, RolldownEngine,
};
use std::fs;

mod common;

/// Shrunk ceiling for this process. Must stay above the entry + one module so the
/// breach is genuinely the *accumulation* of several modules, not one oversized one.
const CEILING_BYTES: usize = 1024 * 1024;
const MODULE_BYTES: usize = 400 * 1024;
const MODULE_COUNT: usize = 4;

#[tokio::test]
async fn total_source_limit_is_enforced() {
    // SAFETY: this is the only test in this binary, and it runs before any thread
    // that could read the environment concurrently (the plugin reads it lazily on
    // the first `module_parsed`, which cannot happen until `bundle` is awaited).
    unsafe {
        std::env::set_var(
            "IMPORT_LENS_MAX_GRAPH_SOURCE_BYTES",
            CEILING_BYTES.to_string(),
        );
    }

    let root = common::temp_workspace("import-lens-graph-source-limit");
    let chunk = format!("export const p = \"{}\";", "A".repeat(MODULE_BYTES));
    let mut entry = String::new();
    for index in 0..MODULE_COUNT {
        entry.push_str(&format!("import './m{index}.js';\n"));
        fs::write(root.join(format!("m{index}.js")), &chunk).expect("module should be written");
    }
    entry.push_str("export const x = 1;");
    fs::write(root.join("entry.js"), &entry).expect("entry should be written");

    let failure = RolldownEngine
        .bundle(BundleRequest {
            entries: vec![BundleEntry {
                entry_path: root.join("entry.js"),
                package_root: root.clone(),
                selection: BundleSelection::Named(vec!["x".to_owned()]),
            }],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
        .expect_err("a graph over the total-source ceiling should fail");

    assert_eq!(failure.stage, "module_graph_limit", "{failure:?}");
    assert!(
        failure
            .message
            .contains(&format!("{CEILING_BYTES} byte total source limit")),
        "{failure:?}"
    );

    fs::remove_dir_all(root).expect("temp workspace should be removed");
}
