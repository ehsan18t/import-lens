use import_lens_daemon::{
    ipc::protocol::{BatchRequest, ImportKind, ImportRequest, ImportRuntime, PROTOCOL_VERSION},
    service::ImportLensService,
};
use std::{
    env,
    path::{Path, PathBuf},
    time::Instant,
};

fn fixture_workspace(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("packages")
        .join(name)
}

fn threshold_ms(base_ms: u128) -> u128 {
    let multiplier = env::var("IMPORT_LENS_PERF_MULTIPLIER")
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .unwrap_or(6)
        .max(1);

    base_ms * multiplier
}

fn uuid_batch(workspace: &Path, request_id: u64) -> BatchRequest {
    BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("app.ts")
            .to_string_lossy()
            .to_string(),
        imports: vec![ImportRequest {
            specifier: "uuid".to_owned(),
            package_name: "uuid".to_owned(),
            version: "13.0.0".to_owned(),
            named: vec!["v4".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    }
}

#[test]
fn fixture_miss_and_cache_hit_stay_under_release_thresholds() {
    let workspace = fixture_workspace("uuid@13.0.0");
    let service = ImportLensService::new(None, false);

    let miss_start = Instant::now();
    let miss = service.handle_batch(uuid_batch(&workspace, 1));
    let miss_ms = miss_start.elapsed().as_millis();

    let hit_start = Instant::now();
    let hit = service.handle_batch(uuid_batch(&workspace, 2));
    let hit_ms = hit_start.elapsed().as_millis();

    assert_eq!(miss.imports[0].error, None);
    assert!(!miss.imports[0].cache_hit);
    assert_eq!(hit.imports[0].error, None);
    assert!(hit.imports[0].cache_hit);

    assert!(
        miss_ms <= threshold_ms(500),
        "fixture cache miss exceeded threshold: {miss_ms}ms",
    );
    assert!(
        hit_ms <= threshold_ms(50),
        "cache hit exceeded threshold: {hit_ms}ms",
    );
}
