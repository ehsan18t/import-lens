//! Direct assets are empty Rolldown modules, but their raw inputs still belong to the graph's
//! aggregate source-byte budget. This test owns a process-local shrunk ceiling so both the success
//! accounting and the pre-read rejection path stay cheap and deterministic.

use std::fs;

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, BundleSelection, ImportRuntime, RolldownEngine,
};

mod common;

const CEILING_BYTES: usize = 64 * 1024;
const ADMITTED_ASSET_BYTES: usize = 16 * 1024;
const REJECTED_ASSET_BYTES: usize = CEILING_BYTES + 1;

async fn bundle(
    root: &std::path::Path,
) -> Result<import_lens_daemon::engine::BundleArtifact, import_lens_daemon::engine::BundleFailure> {
    RolldownEngine
        .bundle(BundleRequest {
            entries: vec![BundleEntry {
                entry_path: root.join("entry.js"),
                package_root: root.to_path_buf(),
                selection: BundleSelection::Named(vec!["value".to_owned()]),
            }],
            runtime: ImportRuntime::default(),
            purpose: BundlePurpose::ImportSize,
        })
        .await
}

#[tokio::test]
async fn direct_assets_are_admitted_and_rejected_by_raw_source_length() {
    // SAFETY: this is the only test in this binary, and the limit is initialized only when the
    // first bundle starts below.
    unsafe {
        std::env::set_var(
            "IMPORT_LENS_MAX_GRAPH_SOURCE_BYTES",
            CEILING_BYTES.to_string(),
        );
    }

    let admitted = common::temp_workspace("import-lens-direct-asset-admitted");
    fs::write(
        admitted.join("entry.js"),
        "import './font.woff2';\nexport const value = 1;\n",
    )
    .expect("entry");
    fs::write(
        admitted.join("font.woff2"),
        vec![0x41; ADMITTED_ASSET_BYTES],
    )
    .expect("font");

    let artifact = bundle(&admitted)
        .await
        .expect("an asset inside the graph ceiling should bundle");
    assert_eq!(artifact.assets.len(), 1, "{artifact:?}");
    assert!(
        artifact.graph_source_bytes >= ADMITTED_ASSET_BYTES,
        "the finalized graph total must include the empty-stubbed asset: {artifact:?}"
    );
    assert!(artifact.graph_source_bytes <= CEILING_BYTES, "{artifact:?}");
    fs::remove_dir_all(admitted).expect("admitted workspace cleanup");

    let rejected = common::temp_workspace("import-lens-direct-asset-rejected");
    fs::write(
        rejected.join("entry.js"),
        "import './font.woff2';\nexport const value = 1;\n",
    )
    .expect("entry");
    fs::write(
        rejected.join("font.woff2"),
        vec![0x42; REJECTED_ASSET_BYTES],
    )
    .expect("font");

    let failure = bundle(&rejected)
        .await
        .expect_err("an asset larger than the aggregate ceiling must fail before admission");
    assert_eq!(failure.stage, "module_graph_limit", "{failure:?}");
    assert!(
        failure
            .message
            .contains(&format!("{CEILING_BYTES} byte total source limit")),
        "{failure:?}"
    );
    let asset_fingerprint = failure
        .read_time_fingerprints
        .iter()
        .find(|fingerprint| fingerprint.path.ends_with("font.woff2"))
        .expect("a deterministic pre-read failure must retain the asset stat fingerprint");
    assert_eq!(asset_fingerprint.len, REJECTED_ASSET_BYTES as u64);
    assert_eq!(asset_fingerprint.content_hash, None);

    fs::remove_dir_all(rejected).expect("rejected workspace cleanup");
}
