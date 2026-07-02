use import_lens_daemon::{
    ipc::protocol::{PROTOCOL_VERSION, WorkspaceReportBudgets, WorkspaceReportRequest},
    service::ImportLensService,
};
use std::{fs, path::PathBuf};

mod common;

fn temp_workspace() -> PathBuf {
    common::temp_workspace("import-lens-report")
}

fn write_report_package(workspace: &std::path::Path) {
    let package_root = workspace.join("node_modules").join("tiny-lib");
    fs::create_dir_all(&package_root).expect("package root");
    fs::write(
        package_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("manifest");
    fs::write(package_root.join("index.js"), "export const value = 1;").expect("entry");
}

#[test]
fn workspace_report_scans_supported_sources_and_skips_node_modules() {
    let workspace = temp_workspace();
    write_report_package(&workspace);
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        workspace.join("src").join("index.ts"),
        "import { value } from 'tiny-lib';\nconsole.log(value);",
    )
    .expect("source file");
    fs::write(
        workspace.join("node_modules").join("ignored.ts"),
        "import { value } from 'tiny-lib';",
    )
    .expect("ignored source");
    let service = ImportLensService::new(None, false);

    let response = service.build_workspace_report(WorkspaceReportRequest {
        message_type: "workspace_report".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 90,
        workspace_root: workspace.to_string_lossy().to_string(),
        budgets: WorkspaceReportBudgets {
            per_import_brotli_bytes: Some(1),
            per_file_brotli_bytes: Some(1),
        },
    });

    fs::remove_dir_all(workspace).expect("workspace cleanup");
    assert_eq!(response.error, None);
    assert_eq!(response.rows.len(), 1, "{response:?}");
    assert_eq!(response.rows[0].package_name, "tiny-lib");
    assert_eq!(response.summary.import_count, 1);
    assert!(response.summary.total_brotli_bytes > 0);
    assert!(response.summary.budget_violation_count > 0);
}
