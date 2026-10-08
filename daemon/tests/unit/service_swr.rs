use super::{ImportLensService, MeasuredImport, revalidated_with_shared_bytes};
use crate::{
    cache::key::cache_key_for_resolved_import,
    ipc::protocol::{
        DetectedImport, FileSizeDocumentRequest, ImportKind, ImportResult, MeasuredSizes,
        ModuleContribution, PROTOCOL_VERSION, RefreshedImportIdentity,
    },
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

#[test]
fn a_revalidated_import_is_pushed_with_its_shared_figure_and_moves_its_siblings() {
    let source = "import def from 'lib';\nimport { named } from 'lib';\nimport other from 'other';";
    let detected = detected_imports_for_document("/w/src/a.ts", source, true, &Default::default())
        .expect("the document should parse");
    let identity = |detected: &DetectedImport| RefreshedImportIdentity {
        specifier: detected.specifier.clone(),
        import_kind: detected.import_kind,
        named: detected.named.clone(),
        runtime: detected.runtime,
    };
    let measured = |specifier: &str, modules: &[(&str, u64)], shared: Option<u64>| {
        let mut result = ImportResult::measured(
            specifier,
            MeasuredSizes {
                raw_bytes: 200,
                minified_bytes: 150,
                gzip_bytes: 80,
                brotli_bytes: 70,
                zstd_bytes: 75,
            },
        );
        result.module_breakdown = Some(
            modules
                .iter()
                .map(|(path, bytes)| ModuleContribution {
                    path: (*path).to_owned(),
                    bytes: *bytes,
                })
                .collect(),
        );
        result.shared_bytes = shared;
        result
    };
    let served = [
        (
            0,
            measured("lib", &[("shared.js", 100), ("def.js", 5)], Some(100)),
        ),
        (
            1,
            measured("lib", &[("shared.js", 100), ("named.js", 7)], Some(100)),
        ),
        (2, measured("other", &[("other.js", 50)], Some(0))),
    ]
    .map(|(index, result)| MeasuredImport {
        result,
        identity: identity(&detected[index]),
    })
    .to_vec();

    let (results, identities) = revalidated_with_shared_bytes(
        served.clone(),
        vec![measured("lib", &[("shared.js", 100), ("def.js", 5)], None)],
        vec![identity(&detected[0])],
    );
    assert_eq!(identities, vec![identity(&detected[0])]);
    assert_eq!(results[0].shared_bytes, Some(100));

    let (results, identities) = revalidated_with_shared_bytes(
        served,
        vec![measured("lib", &[("def.js", 5)], None)],
        vec![identity(&detected[0])],
    );
    assert_eq!(
        identities,
        vec![identity(&detected[1]), identity(&detected[0])],
        "the named import no longer shares anything once the default import stops reaching shared.js"
    );
    assert_eq!(
        results
            .iter()
            .map(|result| result.shared_bytes)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(0)]
    );
}
