//! Freshness inputs must describe the bytes the size was MEASURED from (spec §8.3).
//!
//! Fingerprinting the graph after the build — by re-reading every module from disk —
//! reopens a staleness window: a file edited during the analysis window is recorded
//! with its NEW bytes against a size computed from the OLD ones. The entry then never
//! self-heals, because every later probe re-reads the file, matches the stored hash,
//! and answers `Fresh`, serving the stale size until that file changes again.
//!
//! The plugin now hashes each module's bytes as it reads them, inside the build, so
//! there is no window to lose and no second pass over the graph's bytes. These are the
//! guards for that property — they re-home the two the cutover deleted along with the
//! old engine (`fingerprints_capture_read_time_len_not_post_analysis_stat` and
//! `module_graph_carries_content_hash_for_loaded_modules`).

use import_lens_daemon::engine::{
    BundleEntry, BundlePurpose, BundleRequest, BundleSelection, boundary,
};
use import_lens_daemon::ipc::protocol::ImportRuntime;
use import_lens_daemon::pipeline::resolver::SideEffectsMode;
use std::{fs, path::Path, path::PathBuf};

mod common;

fn write_source(root: &Path, relative_path: &str, source: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("source file should have a parent"))
        .expect("source parent directory should be created");
    fs::write(path, source).expect("source file should be written");
}

fn bundle(
    entry_path: PathBuf,
    package_root: PathBuf,
) -> import_lens_daemon::engine::BundleArtifact {
    boundary::bundle_sync(BundleRequest {
        entries: vec![BundleEntry {
            entry_path,
            package_root,
            selection: BundleSelection::Named(vec!["used".to_owned()]),
            reported_side_effects: SideEffectsMode::False,
        }],
        runtime: ImportRuntime::Component,
        purpose: BundlePurpose::ImportSize,
    })
    .expect("bundle should succeed")
}

/// If any loaded module lacks a read-time fingerprint, the caller must re-read it
/// from disk to fingerprint it — which is exactly the post-analysis pass that reopens
/// the staleness window. For an all-text graph the set must be complete.
#[test]
fn every_loaded_module_carries_a_read_time_fingerprint() {
    let root = common::temp_workspace("import-lens-freshness-readtime");
    write_source(
        &root,
        "node_modules/pkg/package.json",
        r#"{"name":"pkg","version":"1.0.0","module":"./index.js","sideEffects":false}"#,
    );
    write_source(
        &root,
        "node_modules/pkg/index.js",
        "export { used } from \"./dep.js\";\n",
    );
    write_source(
        &root,
        "node_modules/pkg/dep.js",
        "export const used = 1;\nexport const dropped = 2;\n",
    );

    let package_root = root.join("node_modules/pkg");
    let artifact = bundle(package_root.join("index.js"), package_root);

    assert!(
        !artifact.loaded_paths.is_empty(),
        "the build should have loaded modules"
    );
    assert!(
        artifact.unhashed_paths.is_empty(),
        "every loaded module must be fingerprinted as it is read, or the caller has to \
         re-read it after the build and can capture bytes that were never measured. \
         Unhashed: {:?}",
        artifact.unhashed_paths,
    );
    assert_eq!(
        artifact.read_time_fingerprints.len(),
        artifact.loaded_paths.len(),
        "one read-time fingerprint per loaded module"
    );
    assert!(
        artifact
            .read_time_fingerprints
            .iter()
            .all(|fingerprint| fingerprint.content_hash.is_some()),
        "a read-time fingerprint without a content hash degrades freshness to mtime+len"
    );
}

/// A `.ts` module is transformed by the bundler before it reaches the graph, so its
/// in-memory source is NOT its on-disk bytes. Freshness is checked by re-reading the
/// file from disk, so the stored hash must be of the raw disk bytes — hashing the
/// transformed source would make every probe of an unchanged `.ts` file report a
/// mismatch, or worse, mask a real edit.
#[test]
fn a_typescript_dependency_is_hashed_from_raw_disk_bytes_not_transformed_source() {
    let root = common::temp_workspace("import-lens-freshness-ts");
    write_source(
        &root,
        "node_modules/ts-pkg/package.json",
        r#"{"name":"ts-pkg","version":"1.0.0","module":"./index.ts","sideEffects":false}"#,
    );
    // Type annotations are erased by the transform, so the module's in-memory source
    // differs from its bytes on disk.
    let dep_source = "export const used: number = 1;\nexport type Unused = string;\n";
    write_source(&root, "node_modules/ts-pkg/index.ts", dep_source);

    let package_root = root.join("node_modules/ts-pkg");
    let entry = package_root.join("index.ts");
    let artifact = bundle(entry.clone(), package_root);

    let canonical = fs::canonicalize(&entry).expect("entry should canonicalize");
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    let fingerprint = artifact
        .read_time_fingerprints
        .iter()
        .find(|fingerprint| fingerprint.path == normalized)
        .unwrap_or_else(|| {
            panic!(
                "entry should have a read-time fingerprint; got {:?}",
                artifact.read_time_fingerprints
            )
        });

    let raw = fs::read(&entry).expect("entry should be readable");
    assert_eq!(
        fingerprint.content_hash,
        Some(import_lens_daemon::cache::key::content_hash(&raw)),
        "a .ts dependency must hash to its RAW disk bytes; hashing the transformed \
         source would not match what a later freshness probe reads back"
    );
    assert_eq!(
        fingerprint.len,
        raw.len() as u64,
        "the recorded length must be the on-disk length"
    );
}

/// Rolldown normalizes an unset `attach_debug_info` to `Simple`, which wraps every
/// rendered module in `//#region <id>` / `//#endregion` comments. Those bytes land in
/// `raw_bytes`, and `RenderedModule::rendered_length` sums every source in a module's
/// vec — the wrappers included — so they are charged inside the per-module
/// contributions too. That is bundler metadata billed to the user as package cost, on
/// every build. This guard fails if debug attachment is ever re-enabled.
#[test]
fn a_production_chunk_carries_no_bundler_debug_metadata() {
    let root = common::temp_workspace("import-lens-no-debug-info");
    write_source(
        &root,
        "node_modules/pkg/package.json",
        r#"{"name":"pkg","version":"1.0.0","module":"./index.js","sideEffects":false}"#,
    );
    write_source(
        &root,
        "node_modules/pkg/index.js",
        "export { used } from \"./dep.js\";\n",
    );
    write_source(&root, "node_modules/pkg/dep.js", "export const used = 1;\n");

    let package_root = root.join("node_modules/pkg");
    let artifact = bundle(package_root.join("index.js"), package_root);

    assert!(
        !artifact.code.contains("//#region") && !artifact.code.contains("//#endregion"),
        "the chunk must not contain Rolldown debug region comments; they are counted in \
         raw_bytes and in module contributions as if they were package code:\n{}",
        artifact.code
    );
}
