use import_lens_daemon::{
    ipc::protocol::{BatchRequest, ImportKind, ImportRequest, ImportRuntime, PROTOCOL_VERSION},
    service::ImportLensService,
};
use std::{
    env,
    path::{Path, PathBuf},
    time::Instant,
};

mod common;

fn fixture_workspace(name: &str) -> PathBuf {
    common::fixture_workspace(name)
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
#[ignore = "release-only performance smoke run by pnpm test:performance"]
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

#[test]
#[ignore = "release-only performance smoke run by pnpm test:performance"]
fn multi_module_rebundle_stays_under_release_threshold() {
    use std::fs;
    let workspace = common::temp_workspace("import-lens-perf-bundle");
    let pkg = workspace.join("node_modules").join("multi-lib");
    fs::create_dir_all(&pkg).expect("pkg dir");
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        pkg.join("package.json"),
        r#"{"name":"multi-lib","version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest");
    let mut index = String::new();
    for i in 0..40 {
        fs::write(
            pkg.join(format!("leaf{i}.js")),
            format!("const base{i} = {i};\nexport const fn{i} = () => base{i} + 1;\n"),
        )
        .expect("leaf");
        index.push_str(&format!("export {{ fn{i} }} from './leaf{i}.js';\n"));
    }
    fs::write(pkg.join("index.js"), index).expect("index");

    let service = ImportLensService::new(None, false);
    let document = workspace.join("src").join("app.ts");
    let start = Instant::now();
    for i in 0..40 {
        let request = BatchRequest {
            version: PROTOCOL_VERSION,
            request_id: i,
            workspace_root: workspace.to_string_lossy().to_string(),
            active_document_path: document.to_string_lossy().to_string(),
            imports: vec![ImportRequest {
                specifier: "multi-lib".to_owned(),
                package_name: "multi-lib".to_owned(),
                version: "1.0.0".to_owned(),
                named: vec![format!("fn{i}")],
                import_kind: ImportKind::Named,
                runtime: ImportRuntime::Component,
            }],
            streaming: false,
        };
        let response = service.handle_batch(request);
        assert_eq!(response.imports[0].error, None, "{:?}", response.imports[0]);
    }
    let elapsed_ms = start.elapsed().as_millis();

    fs::remove_dir_all(&workspace).expect("cleanup");
    eprintln!("multi_module_rebundle: {elapsed_ms}ms for 40 re-bundles");
    assert!(
        elapsed_ms <= threshold_ms(4000),
        "multi-module re-bundle exceeded threshold: {elapsed_ms}ms"
    );
}

#[test]
#[ignore = "release-only performance smoke run by pnpm test:performance"]
fn warm_reanalysis_of_multi_module_dependency_stays_under_threshold() {
    use std::fs;
    let workspace = common::temp_workspace("import-lens-perf-warm");
    let pkg = workspace.join("node_modules").join("wide-lib");
    fs::create_dir_all(&pkg).expect("pkg dir");
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        pkg.join("package.json"),
        r#"{"name":"wide-lib","version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest");
    let mut index = String::new();
    for i in 0..60 {
        fs::write(
            pkg.join(format!("leaf{i}.js")),
            format!("export const fn{i} = () => {i};\n"),
        )
        .expect("leaf");
        index.push_str(&format!("export {{ fn{i} }} from './leaf{i}.js';\n"));
    }
    fs::write(pkg.join("index.js"), index).expect("index");

    let service = ImportLensService::new(None, false);
    let document = workspace.join("src").join("app.ts");
    let batch = |request_id: u64| BatchRequest {
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: document.to_string_lossy().to_string(),
        imports: vec![ImportRequest {
            specifier: "wide-lib".to_owned(),
            package_name: "wide-lib".to_owned(),
            version: "1.0.0".to_owned(),
            named: vec!["fn0".to_owned()],
            import_kind: ImportKind::Named,
            runtime: ImportRuntime::Component,
        }],
        streaming: false,
    };

    assert_eq!(service.handle_batch(batch(0)).imports[0].error, None);
    let start = Instant::now();
    for i in 1..=50 {
        let response = service.handle_batch(batch(i));
        assert!(response.imports[0].cache_hit, "expected warm hit");
    }
    let elapsed_ms = start.elapsed().as_millis();

    fs::remove_dir_all(&workspace).expect("cleanup");
    eprintln!("warm_reanalysis: {elapsed_ms}ms for 50 hits");
    assert!(
        elapsed_ms <= threshold_ms(2000),
        "warm re-analysis exceeded threshold: {elapsed_ms}ms"
    );
}
