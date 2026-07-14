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

/// Two imports of one file that reach the same transitive module.
fn write_shared_report_packages(workspace: &std::path::Path) {
    let util_root = workspace.join("node_modules").join("shared-util");
    fs::create_dir_all(&util_root).expect("shared util root");
    fs::write(
        util_root.join("package.json"),
        r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
    )
    .expect("shared util manifest");
    fs::write(
        util_root.join("index.js"),
        "export const util = 'a shared utility payload big enough to see in the breakdown';",
    )
    .expect("shared util entry");

    for package_name in ["left-lib", "right-lib"] {
        let package_root = workspace.join("node_modules").join(package_name);
        fs::create_dir_all(&package_root).expect("package root");
        fs::write(
            package_root.join("package.json"),
            r#"{"version":"1.0.0","module":"index.js","sideEffects":false}"#,
        )
        .expect("package manifest");
        let export_name = package_name.replace("-lib", "");
        fs::write(
            package_root.join("index.js"),
            format!("import {{ util }} from 'shared-util';\nexport const {export_name} = util;"),
        )
        .expect("package entry");
    }
}

/// The report's two sharing figures, end to end through a real build.
///
/// A row's `shared_bytes` is a relation between two imports of the SAME file, and the report gets it
/// from `annotate_ready_items` on the shared analysis path — this pins that, because the report is
/// the one consumer that does not call `annotate_shared_bytes` itself, and a row whose sharing fell
/// to `unwrap_or_default()` would print **0 B**: a zero nobody measured, and the false claim
/// *nothing in this file is shared*.
///
/// And the **Shared Modules** group carries the two quantities that used to be one: the module's own
/// size, and the sum across the imports that reach it. Both imports here pull the whole of
/// `shared-util`, so the second must exceed the first — which is precisely the arithmetic that was
/// being rendered as the module's "Total Bytes" (ADR-0004).
#[test]
fn report_rows_carry_the_shared_bytes_of_their_own_file() {
    let workspace = temp_workspace();
    write_shared_report_packages(&workspace);
    fs::create_dir_all(workspace.join("src")).expect("src dir");
    fs::write(
        workspace.join("src").join("index.ts"),
        "import { left } from 'left-lib';\nimport { right } from 'right-lib';\nconsole.log(left, right);",
    )
    .expect("source file");
    let service = ImportLensService::new(None, false);

    let response = service.build_workspace_report(WorkspaceReportRequest {
        message_type: "workspace_report".to_owned(),
        version: PROTOCOL_VERSION,
        request_id: 91,
        workspace_root: workspace.to_string_lossy().to_string(),
        budgets: WorkspaceReportBudgets {
            per_import_brotli_bytes: None,
        },
    });

    fs::remove_dir_all(workspace).expect("workspace cleanup");
    assert_eq!(response.error, None);
    assert_eq!(response.rows.len(), 2, "{response:?}");
    assert!(
        response.rows.iter().all(|row| row.shared_bytes > 0),
        "both imports pull shared-util, so both share bytes with a sibling: {:?}",
        response
            .rows
            .iter()
            .map(|row| (&row.specifier, row.shared_bytes))
            .collect::<Vec<_>>()
    );

    let group = response
        .summary
        .shared_modules
        .iter()
        .find(|group| group.module_path.contains("shared-util"))
        .expect("shared-util is reached by both imports");
    assert_eq!(group.count, 2);
    assert!(
        group.combined_import_cost_bytes > group.module_bytes,
        "two sites pay for one module: the module's own size, and the sum across the sites"
    );
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
        },
    });

    fs::remove_dir_all(workspace).expect("workspace cleanup");
    assert_eq!(response.error, None);
    assert_eq!(response.rows.len(), 1, "{response:?}");
    assert_eq!(response.rows[0].package_name, "tiny-lib");
    assert_eq!(response.summary.import_count, 1);
    assert!(response.summary.combined_import_cost_brotli_bytes > 0);
    assert!(response.summary.budget_violation_count > 0);
}
