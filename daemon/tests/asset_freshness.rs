//! Asset measurements must consume the exact byte snapshot the engine fingerprinted.
//!
//! Mutating a file after `bundle_sync` returns is a deterministic stand-in for an editor or
//! installer changing it between the engine read and post-build asset processing. Reopening the
//! path here binds the old fingerprint to a size derived from new bytes and can make that wrong
//! result look fresh forever.

use std::fs;
use std::path::{Path, PathBuf};

use import_lens_daemon::cache::key::{
    FileFingerprint, Freshness, check_fingerprint, content_hash, fingerprints_are_reusable,
};
use import_lens_daemon::engine::{
    AssetKind, BundleArtifact, BundleEntry, BundlePurpose, BundleRequest, BundleSelection, boundary,
};
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRequest, ImportResult, ImportRuntime};
use import_lens_daemon::pipeline::analyze::{
    AnalysisContext, FingerprintSource, analyze_resolved_import_with_dependencies,
};
use import_lens_daemon::pipeline::assets::process_assets_bounded;
use import_lens_daemon::pipeline::resolver::resolve_package_entry;

mod common;

fn write_file(root: &Path, relative_path: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
        .expect("fixture parent should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn bundle(package_root: PathBuf) -> BundleArtifact {
    boundary::bundle_sync(BundleRequest {
        entries: vec![BundleEntry {
            entry_path: package_root.join("index.js"),
            package_root,
            selection: BundleSelection::Named(vec!["used".to_owned()]),
        }],
        runtime: ImportRuntime::Component,
        purpose: BundlePurpose::ImportSize,
    })
    .expect("asset fixture should bundle")
}

#[test]
fn direct_binary_assets_use_the_snapshot_captured_by_the_engine() {
    const FONT_A: usize = 73;
    const WASM_A: usize = 109;

    let root = common::temp_workspace("import-lens-asset-snapshot-binary");
    let package_root = root.join("node_modules").join("snapshot-lib");
    write_file(
        &package_root,
        "package.json",
        r#"{"name":"snapshot-lib","version":"1.0.0","module":"index.js"}"#,
    );
    write_file(
        &package_root,
        "index.js",
        "import './probe.woff2';\nimport './probe.wasm';\nexport const used = 1;\n",
    );
    write_file(&package_root, "probe.woff2", vec![0x31; FONT_A]);
    write_file(&package_root, "probe.wasm", vec![0x42; WASM_A]);

    let artifact = bundle(package_root.clone());
    for asset in &artifact.assets {
        let expected_hash = match asset.kind {
            AssetKind::Font => content_hash(&[0x31; FONT_A]),
            AssetKind::Wasm => content_hash(&[0x42; WASM_A]),
            AssetKind::Css => panic!("the binary fixture should not collect CSS"),
        };
        assert_eq!(
            asset.fingerprint.content_hash,
            Some(expected_hash),
            "the retained asset fingerprint must hash its original bytes: {asset:?}"
        );
        assert_eq!(
            artifact
                .read_time_fingerprints
                .iter()
                .find(|fingerprint| fingerprint.path == asset.fingerprint.path),
            Some(&asset.fingerprint),
            "the artifact freshness map and retained asset must share one snapshot"
        );
    }

    // The path now contains different bytes. Processing this artifact must remain a function of
    // the snapshot that produced the artifact, not of when post-processing happens to run.
    write_file(&package_root, "probe.woff2", vec![0x53; FONT_A * 10]);
    write_file(&package_root, "probe.wasm", vec![0x64; WASM_A * 10]);
    let processed = process_assets_bounded(
        artifact.assets.clone(),
        artifact.graph_source_bytes,
        artifact.loaded_paths.clone(),
    )
    .expect("the production asset path must admit this fixture");

    let font = processed
        .contributions
        .iter()
        .find(|contribution| contribution.kind == AssetKind::Font)
        .expect("the captured font should be counted");
    let wasm = processed
        .contributions
        .iter()
        .find(|contribution| contribution.kind == AssetKind::Wasm)
        .expect("the captured wasm should be counted");
    assert_eq!(font.raw_bytes, FONT_A as u64, "{font:?}");
    assert_eq!(font.minified_bytes, FONT_A as u64, "{font:?}");
    assert_eq!(wasm.raw_bytes, WASM_A as u64, "{wasm:?}");
    assert_eq!(wasm.minified_bytes, WASM_A as u64, "{wasm:?}");

    fs::remove_dir_all(root).ok();
}

#[test]
fn a_top_level_stylesheet_uses_the_snapshot_captured_by_the_engine() {
    let root = common::temp_workspace("import-lens-asset-snapshot-css");
    let package_root = root.join("node_modules").join("snapshot-css-lib");
    write_file(
        &package_root,
        "package.json",
        r#"{"name":"snapshot-css-lib","version":"1.0.0","module":"index.js"}"#,
    );
    write_file(
        &package_root,
        "index.js",
        "import './styles.css';\nexport const used = 1;\n",
    );
    write_file(
        &package_root,
        "styles.css",
        ".snapshot { color: red; padding: 1px; }\n",
    );

    let artifact = bundle(package_root.clone());
    let before_edit = process_assets_bounded(
        artifact.assets.clone(),
        artifact.graph_source_bytes,
        artifact.loaded_paths.clone(),
    )
    .expect("the production asset path must admit this fixture");

    write_file(
        &package_root,
        "styles.css",
        ".snapshot { color: rebeccapurple; padding: 123456px; margin: 654321px; }\n",
    );
    let after_edit = process_assets_bounded(
        artifact.assets.clone(),
        artifact.graph_source_bytes,
        artifact.loaded_paths.clone(),
    )
    .expect("the production asset path must admit this fixture");

    assert_eq!(
        before_edit.contributions, after_edit.contributions,
        "processing one bundle artifact twice must not reopen its stylesheet path"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn css_children_and_local_resources_carry_read_time_fingerprints() {
    let root = common::temp_workspace("import-lens-asset-snapshot-css-children");
    let package_root = root.join("node_modules").join("css-child-lib");
    let child_bytes = b"@font-face { font-family: Probe; src: url('./probe.woff2'); }\n\
                        .child { font-family: Probe; color: red; }\n";
    let font_bytes = vec![0x75; 3072];
    write_file(
        &package_root,
        "package.json",
        r#"{"name":"css-child-lib","version":"1.0.0","module":"index.js"}"#,
    );
    write_file(
        &package_root,
        "index.js",
        "import './styles.css';\nexport const used = 1;\n",
    );
    write_file(
        &package_root,
        "styles.css",
        "@import './nested/child.css';\n.entry { color: blue; }\n",
    );
    write_file(&package_root, "nested/child.css", child_bytes);
    write_file(&package_root, "nested/probe.woff2", &font_bytes);

    let context = AnalysisContext {
        workspace_root: root.clone(),
        active_document_path: root.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "css-child-lib".to_owned(),
        package_name: "css-child-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };
    let resolved = resolve_package_entry(&context.active_document_path, &request)
        .expect("fixture package should resolve");
    let (result, source) = analyze_resolved_import_with_dependencies(&context, &request, resolved);
    assert_eq!(result.error, None, "{result:?}");

    let FingerprintSource::ReadTime {
        fingerprints,
        stat_paths,
    } = source.expect("a successful engine build should return freshness inputs");
    let child =
        fs::canonicalize(package_root.join("nested/child.css")).expect("child should canonicalize");
    let font = fs::canonicalize(package_root.join("nested/probe.woff2"))
        .expect("font should canonicalize");

    for (path, authored_bytes) in [
        (&child, child_bytes.as_slice()),
        (&font, font_bytes.as_slice()),
    ] {
        let normalized = path.to_string_lossy().replace('\\', "/");
        let fingerprint = fingerprints
            .iter()
            .find(|fingerprint| fingerprint.path == normalized)
            .unwrap_or_else(|| panic!("{} must be fingerprinted at read time", path.display()));
        assert_eq!(
            fingerprint.content_hash,
            Some(content_hash(authored_bytes)),
            "the fingerprint must hash the bytes that fed the size"
        );
        assert!(
            !stat_paths.iter().any(|deferred| deferred == path),
            "a measured asset must not be reopened later for freshness"
        );
    }

    write_file(
        &package_root,
        "nested/child.css",
        ".child { color: rebeccapurple; padding: 12345px; }\n",
    );
    write_file(
        &package_root,
        "nested/probe.woff2",
        vec![0x16; font_bytes.len() + 17],
    );
    for path in [&child, &font] {
        let normalized = path.to_string_lossy().replace('\\', "/");
        let fingerprint = fingerprints
            .iter()
            .find(|fingerprint| fingerprint.path == normalized)
            .expect("fingerprint should still be present");
        assert_eq!(
            check_fingerprint(fingerprint),
            Freshness::Stale,
            "an edit after analysis must invalidate {}",
            path.display()
        );
    }

    fs::remove_dir_all(root).ok();
}

fn analyze_fixture(root: &Path, package_name: &str) -> (ImportResult, Vec<FileFingerprint>) {
    let context = AnalysisContext {
        workspace_root: root.to_path_buf(),
        active_document_path: root.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: package_name.to_owned(),
        package_name: package_name.to_owned(),
        version: "1.0.0".to_owned(),
        named: vec!["used".to_owned()],
        import_kind: ImportKind::Named,
        runtime: ImportRuntime::Component,
    };
    let resolved = resolve_package_entry(&context.active_document_path, &request)
        .expect("fixture package should resolve");
    let (result, source) = analyze_resolved_import_with_dependencies(&context, &request, resolved);
    let FingerprintSource::ReadTime { fingerprints, .. } =
        source.expect("an engine build should return freshness inputs");
    (result, fingerprints)
}

/// A loader suffix is not part of the file name, so the module it names is the file `load` read and
/// fingerprinted. Keyed under the suffixed id instead, it has no read-time fingerprint and the
/// result can never be reused by any cache.
#[test]
fn an_asset_imported_with_a_loader_suffix_is_fingerprinted_under_its_file() {
    let root = common::temp_workspace("import-lens-asset-loader-suffix");
    let package_root = root.join("node_modules").join("suffix-lib");
    write_file(
        &package_root,
        "package.json",
        r#"{"name":"suffix-lib","version":"1.0.0","module":"index.js"}"#,
    );
    write_file(
        &package_root,
        "index.js",
        "import fontUrl from './probe.woff2?url';\nexport const used = () => fontUrl;\n",
    );
    write_file(&package_root, "probe.woff2", vec![0x3c; 1024]);

    let (result, fingerprints) = analyze_fixture(&root, "suffix-lib");
    fs::remove_dir_all(&root).ok();

    assert_eq!(result.error, None, "{result:?}");
    assert!(
        fingerprints_are_reusable(&fingerprints),
        "a measured suffixed asset must not make the result uncacheable: {fingerprints:?}"
    );
    assert!(
        fingerprints
            .iter()
            .all(|fingerprint| !fingerprint.path.ends_with("?url")),
        "no module may be keyed under its loader suffix: {fingerprints:?}"
    );
}

/// A bare asset subpath the resolver refuses is a fact about the package graph: an uninstalled
/// optional peer, or a file another package's `exports` map declines to expose. Neither is a
/// filesystem moment, so neither may surface as `asset_io` or make the result uncacheable; both are
/// the disclosed import boundary every other refused bare subpath gets.
#[test]
fn a_refused_bare_asset_subpath_is_a_disclosed_boundary_not_an_io_failure() {
    let root = common::temp_workspace("import-lens-asset-bare-refused");
    write_file(
        &root.join("node_modules").join("closed-pkg"),
        "package.json",
        r#"{"name":"closed-pkg","version":"1.0.0","exports":{".":"./index.js"}}"#,
    );
    write_file(
        &root.join("node_modules").join("closed-pkg"),
        "index.js",
        "export const closed = 1;\n",
    );
    write_file(
        &root.join("node_modules").join("closed-pkg"),
        "dist/x.css",
        ".closed { color: red; }\n",
    );
    for (package_name, specifier) in [
        ("missing-peer-lib", "not-installed-peer/dist/peer.min.css"),
        ("denied-subpath-lib", "closed-pkg/dist/x.css"),
    ] {
        let package_root = root.join("node_modules").join(package_name);
        write_file(
            &package_root,
            "package.json",
            format!(r#"{{"name":"{package_name}","version":"1.0.0","module":"index.js"}}"#),
        );
        write_file(
            &package_root,
            "index.js",
            format!("import '{specifier}';\nexport const used = 1;\n"),
        );

        let (result, fingerprints) = analyze_fixture(&root, package_name);

        assert_eq!(result.error, None, "{specifier}: {result:?}");
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == "asset_io"),
            "{specifier}: a refused bare specifier is not a filesystem incident: {result:?}"
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.stage == "external"
                    && diagnostic.message.contains(specifier)),
            "{specifier}: the boundary must be disclosed: {result:?}"
        );
        assert!(
            fingerprints_are_reusable(&fingerprints),
            "{specifier}: a deterministic boundary must stay cacheable: {fingerprints:?}"
        );
    }
    fs::remove_dir_all(&root).ok();
}

/// An image a counted stylesheet references is disclosed with its byte count, so that count is a
/// cached fact about the file and must expire when the file is resized or removed.
#[test]
fn a_css_referenced_image_disclosure_expires_when_the_image_changes() {
    let root = common::temp_workspace("import-lens-asset-css-image");
    let package_root = root.join("node_modules").join("sprite-lib");
    write_file(
        &package_root,
        "package.json",
        r#"{"name":"sprite-lib","version":"1.0.0","module":"index.js"}"#,
    );
    write_file(
        &package_root,
        "index.js",
        "import './styles.css';\nexport const used = 1;\n",
    );
    write_file(
        &package_root,
        "styles.css",
        ".icon { background: url('./sprite.png'); }\n",
    );
    write_file(&package_root, "sprite.png", vec![0x89; 4000]);

    let (result, fingerprints) = analyze_fixture(&root, "sprite-lib");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "uncounted_assets"
                && diagnostic.message.contains("4000")),
        "the image is disclosed with its size: {result:?}"
    );
    let sprite = fs::canonicalize(package_root.join("sprite.png"))
        .expect("sprite should canonicalize")
        .to_string_lossy()
        .replace('\\', "/");
    let fingerprint = fingerprints
        .iter()
        .find(|fingerprint| fingerprint.path == sprite)
        .unwrap_or_else(|| {
            panic!("the disclosed image must be a freshness input: {fingerprints:?}")
        })
        .clone();
    assert_eq!(check_fingerprint(&fingerprint), Freshness::Fresh);

    write_file(&package_root, "sprite.png", vec![0x89; 40_000]);
    assert_eq!(
        check_fingerprint(&fingerprint),
        Freshness::Stale,
        "resizing the image must expire its cached disclosure"
    );
    fs::remove_dir_all(&root).ok();
}
