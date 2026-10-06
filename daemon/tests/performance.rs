use import_lens_daemon::{
    ipc::protocol::{AnalyzeDocumentRequest, PROTOCOL_VERSION},
    service::ImportLensService,
};
use std::{
    env,
    path::{Path, PathBuf},
    time::Instant,
};

mod common;

use common::documents::analyze_document;

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

fn document(workspace: &Path, request_id: u64, source: String) -> AnalyzeDocumentRequest {
    AnalyzeDocumentRequest {
        message_type: "analyze_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join("app.ts")
            .to_string_lossy()
            .to_string(),
        source,
    }
}

fn uuid_document(workspace: &Path, request_id: u64) -> AnalyzeDocumentRequest {
    document(
        workspace,
        request_id,
        "import { v4 } from 'uuid';".to_owned(),
    )
}

#[test]
#[ignore = "release-only performance smoke run by pnpm test:performance"]
fn fixture_miss_and_cache_hit_stay_under_release_thresholds() {
    let workspace = fixture_workspace("uuid@13.0.0");
    let service = ImportLensService::new(None, false);

    let miss_start = Instant::now();
    let miss = analyze_document(&service, uuid_document(&workspace, 1));
    let miss_ms = miss_start.elapsed().as_millis();

    let hit_start = Instant::now();
    let hit = analyze_document(&service, uuid_document(&workspace, 2));
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
    let start = Instant::now();
    for i in 0..40 {
        let response = analyze_document(
            &service,
            document(
                &workspace,
                i,
                format!("import {{ fn{i} }} from 'multi-lib';"),
            ),
        );
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
    let wide = |request_id: u64| {
        document(
            &workspace,
            request_id,
            "import { fn0 } from 'wide-lib';".to_owned(),
        )
    };

    assert_eq!(analyze_document(&service, wide(0)).imports[0].error, None);
    let start = Instant::now();
    for i in 1..=50 {
        let response = analyze_document(&service, wide(i));
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
