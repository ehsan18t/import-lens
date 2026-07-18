//! CSS-discovered resources must share the graph's aggregate source-byte ceiling.
//!
//! This lives in its own integration-test process because the production limit override is read
//! lazily from the environment. Keeping one test here makes the shrunk ceiling deterministic.

use std::fs;

use import_lens_daemon::cache::key::fingerprints_are_current;
use import_lens_daemon::ipc::protocol::{
    BatchRequest, ImportKind, ImportRequest, ImportResult, ImportRuntime, MeasuredSizes,
    PROTOCOL_VERSION,
};
use import_lens_daemon::pipeline::analyze::{
    AnalysisContext, FingerprintSource, analyze_import, analyze_resolved_import_with_dependencies,
};
use import_lens_daemon::pipeline::file_size::{SizedImport, compute_file_size};
use import_lens_daemon::pipeline::resolver::resolve_package_entry;
use import_lens_daemon::service::ImportLensService;

mod common;

const CEILING_BYTES: usize = 64 * 1024;

#[test]
fn a_css_referenced_asset_cannot_escape_the_graph_source_ceiling() {
    // SAFETY: this is the only test in this binary, and the limit is initialized only when the
    // first bundle starts below.
    unsafe {
        std::env::set_var(
            "IMPORT_LENS_MAX_GRAPH_SOURCE_BYTES",
            CEILING_BYTES.to_string(),
        );
    }

    let workspace = common::temp_workspace("import-lens-css-resource-limit");
    let package_root = workspace.join("node_modules").join("resource-limit-lib");
    fs::create_dir_all(&package_root).expect("package root");
    fs::write(
        package_root.join("package.json"),
        r#"{"name":"resource-limit-lib","version":"1.0.0","module":"index.js","sideEffects":["*.css"]}"#,
    )
    .expect("manifest");
    fs::write(
        package_root.join("index.js"),
        "import './styles.css';\nexport const value = 1;\n",
    )
    .expect("entry");
    fs::write(
        package_root.join("styles.css"),
        "@font-face { font-family: Probe; src: url('./oversized.woff2'); }\n\
         .entry { font-family: Probe; background: url('./must-not-read.wasm'); }\n",
    )
    .expect("stylesheet");
    fs::write(
        package_root.join("oversized.woff2"),
        vec![0x51; CEILING_BYTES + 1],
    )
    .expect("font");
    fs::write(package_root.join("must-not-read.wasm"), [0x62; 16]).expect("later resource");

    let context = AnalysisContext {
        workspace_root: workspace.clone(),
        active_document_path: workspace.join("src").join("index.ts"),
    };
    let request = ImportRequest {
        specifier: "resource-limit-lib".to_owned(),
        package_name: "resource-limit-lib".to_owned(),
        version: "1.0.0".to_owned(),
        named: Vec::new(),
        import_kind: ImportKind::Namespace,
        runtime: ImportRuntime::Component,
    };
    let result = analyze_import(&context, &request);
    assert_eq!(
        result.sizes(),
        None,
        "a breached graph has no size: {result:?}"
    );
    assert_eq!(
        result.unmeasured_stage(),
        Some("module_graph_limit"),
        "CSS resources must be admitted under the same typed graph limit: {result:?}"
    );
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|message| message.contains(&CEILING_BYTES.to_string())),
        "the failure should name the aggregate byte ceiling: {result:?}"
    );

    let resolved = resolve_package_entry(&context.active_document_path, &request)
        .expect("resource-limit package should resolve");
    let (_, failure_source) =
        analyze_resolved_import_with_dependencies(&context, &request, resolved);
    let Some(FingerprintSource::ReadTime { fingerprints, .. }) = failure_source else {
        panic!("a durable asset-limit failure must carry its exact freshness inputs");
    };
    assert!(
        fingerprints_are_current(&fingerprints),
        "the captured failure inputs should initially be current: {fingerprints:?}"
    );
    assert!(
        !fingerprints
            .iter()
            .any(|fingerprint| fingerprint.path.ends_with("must-not-read.wasm")),
        "dependency discovery must stop reading after the fatal limit breach: {fingerprints:?}"
    );

    let prior_measurement = ImportResult::measured(
        request.specifier.clone(),
        MeasuredSizes {
            raw_bytes: 10,
            minified_bytes: 8,
            gzip_bytes: 6,
            brotli_bytes: 5,
            zstd_bytes: 7,
        },
    );
    let file_cost = compute_file_size(
        &context,
        &[SizedImport::installed(
            request.clone(),
            Some(prior_measurement),
        )],
    );
    assert!(
        file_cost.degraded,
        "a combined asset-limit failure must be an explicitly degraded fallback: {file_cost:?}"
    );
    assert!(
        !file_cost.is_cacheable(),
        "a per-import fallback is not a File Cost measurement: {file_cost:?}"
    );
    assert!(
        file_cost
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.stage == "module_graph_limit"),
        "File Cost must retain the typed asset-limit stage: {file_cost:?}"
    );

    let service = ImportLensService::new(None, false);
    let batch = |request_id| BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().into_owned(),
        active_document_path: context.active_document_path.to_string_lossy().into_owned(),
        imports: vec![request.clone()],
        streaming: false,
    };
    let first_service_result = service.handle_batch(batch(1)).imports.remove(0);
    assert_eq!(
        first_service_result.unmeasured_stage(),
        Some("module_graph_limit")
    );
    assert!(!first_service_result.cache_hit, "first analysis must miss");
    let cached_failure = service.handle_batch(batch(2)).imports.remove(0);
    assert!(
        cached_failure.cache_hit,
        "a deterministic limit result should be reusable while its inputs are unchanged: \
         {cached_failure:?}"
    );

    fs::write(package_root.join("oversized.woff2"), [0x51; 32]).expect("shrink the offending font");
    assert!(
        !fingerprints_are_current(&fingerprints),
        "fixing only the offending asset must expire a cached deterministic rejection"
    );
    let recovered = analyze_import(&context, &request);
    assert!(
        recovered.sizes().is_some(),
        "the same import should become measurable after the asset is fixed: {recovered:?}"
    );
    let refreshed = service.handle_batch(batch(3)).imports.remove(0);
    assert!(
        refreshed.sizes().is_some(),
        "the service cache must re-run after only the offending asset changes: {refreshed:?}"
    );
    assert!(
        !refreshed.cache_hit,
        "the stale deterministic failure must not be returned as a hit: {refreshed:?}"
    );

    fs::remove_dir_all(workspace).expect("workspace cleanup");
}
