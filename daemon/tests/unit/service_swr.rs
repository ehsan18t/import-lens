use super::ImportLensService;
use crate::{
    cache::key::cache_key_for_resolved_import,
    ipc::protocol::{FileSizeDocumentRequest, ImportKind, PROTOCOL_VERSION},
    pipeline::resolver::resolve_package_entry,
    service::{detected_imports_for_document, import_request_for_detected},
};
use std::{collections::HashSet, fs, path::Path};

fn temp_workspace() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "il-service-swr-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn write_package(workspace: &Path) {
    let package_root = workspace.join("node_modules").join("shared-swr-lib");
    fs::create_dir_all(&package_root).expect("package root should be created");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("package manifest should be written");
    fs::write(package_root.join("index.js"), "export const value = 1;")
        .expect("entry should be written");
}

fn request(workspace: &Path, document_name: &str, generation: u64) -> FileSizeDocumentRequest {
    FileSizeDocumentRequest {
        message_type: "file_size_document".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: generation,
        workspace_root: workspace.to_string_lossy().to_string(),
        active_document_path: workspace
            .join("src")
            .join(document_name)
            .to_string_lossy()
            .to_string(),
        source: "import { value } from 'shared-swr-lib';".to_owned(),
        force_fresh: false,
        analysis_generation: Some(generation),
    }
}

#[test]
fn revalidate_document_sizes_claim_is_scoped_to_document_delivery() {
    let workspace = temp_workspace();
    write_package(&workspace);
    let service = ImportLensService::new(None, false);
    let first_request = request(&workspace, "a.ts", 1);
    let second_request = request(&workspace, "b.ts", 2);

    let detected = detected_imports_for_document(
        &first_request.active_document_path,
        &first_request.source,
        true,
        &Default::default(),
    )
    .expect("document import should parse");
    let detected_import = detected.first().expect("one import should be detected");
    assert!(matches!(detected_import.import_kind, ImportKind::Named));
    let import_request = import_request_for_detected(
        Path::new(&first_request.active_document_path),
        detected_import,
    )
    .expect("detected import should become an import request");
    let resolved = resolve_package_entry(
        Path::new(&first_request.active_document_path),
        &import_request,
    )
    .expect("package should resolve");
    let raw_cache_key = cache_key_for_resolved_import(&import_request, &resolved);
    let cache = service
        .cache_registry
        .cache_for_root(Path::new(&first_request.workspace_root));
    let _held_raw_claim = cache
        .begin_revalidation(&raw_cache_key)
        .expect("test setup should hold the old raw-key claim");

    let stale = HashSet::from(["shared-swr-lib".to_owned()]);
    let refreshed = service.revalidate_document_sizes(&second_request, &stale, || true);

    fs::remove_dir_all(&workspace).ok();
    assert!(
        refreshed.is_some(),
        "a raw cache-key claim from another document must not starve this document's SWR push"
    );
}
