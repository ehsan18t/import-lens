use crate::{
    ipc::protocol::{
        ConfidenceLevel, ImportDiagnostic, ImportRequest, ImportResult, MeasuredSizes,
    },
    pipeline::{resolver::find_package_root, util::should_skip_package_directory},
};
use std::{
    fs::{self, DirEntry},
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
struct DeclarationOnlyScan {
    has_declaration_file: bool,
    has_runtime_file: bool,
    hit_cap: bool,
}

impl DeclarationOnlyScan {
    fn is_declaration_only(&self) -> bool {
        self.has_declaration_file && !self.has_runtime_file && !self.hit_cap
    }
}

const DECLARATION_ONLY_MAX_FILES: usize = 10_000;

pub fn declaration_only_package_result(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Option<ImportResult> {
    let package_root = find_package_root(active_document_path, &request.package_name).ok()?;
    let scan = scan_declaration_only_package(&package_root);

    if !scan.is_declaration_only() {
        return None;
    }

    // MEASURED, not Unmeasured (ADR-0006). A declarations-only package genuinely ships zero
    // runtime bytes: nothing failed, there was simply nothing to build. `Some(0)` stays
    // unambiguous because the `types_only` diagnostic stage identifies it — this is the one place
    // in the daemon where a zero is an answer rather than the absence of one.
    let mut result = ImportResult::measured(request.specifier.clone(), MeasuredSizes::ZERO);
    result.truly_treeshakeable = true;
    result.confidence = ConfidenceLevel::High;
    result.confidence_reasons =
        vec!["Package contains declaration files only and no runtime source files.".to_owned()];
    result.diagnostics = vec![ImportDiagnostic {
        stage: crate::pipeline::stage::TYPES_ONLY.to_owned(),
        message: "package contains declarations only; zero runtime cost".to_owned(),
        details: vec![
            format!("specifier: {}", request.specifier),
            format!("package: {}", request.package_name),
            format!("package_root: {}", package_root.display()),
        ],
    }];
    result.module_breakdown = Some(Vec::new());

    Some(result)
}

fn scan_declaration_only_package(package_root: &Path) -> DeclarationOnlyScan {
    let mut scan = DeclarationOnlyScan::default();
    let mut stack = vec![package_root.to_path_buf()];
    let mut files = 0_usize;

    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            scan_entry(entry, &mut stack, &mut files, &mut scan);

            if scan.has_runtime_file || scan.hit_cap {
                return scan;
            }
        }
    }

    scan
}

fn scan_entry(
    entry: DirEntry,
    stack: &mut Vec<PathBuf>,
    files: &mut usize,
    scan: &mut DeclarationOnlyScan,
) {
    let Ok(file_type) = entry.file_type() else {
        return;
    };
    let path = entry.path();

    if file_type.is_dir() {
        if !should_skip_package_directory(&path) {
            stack.push(path);
        }
        return;
    }

    if !file_type.is_file() {
        return;
    }

    *files += 1;
    if *files > DECLARATION_ONLY_MAX_FILES {
        scan.hit_cap = true;
        return;
    }

    if is_declaration_file(&path) {
        scan.has_declaration_file = true;
        return;
    }

    if is_runtime_source_file(&path) {
        scan.has_runtime_file = true;
    }
}

fn is_declaration_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower_name = name.to_ascii_lowercase();
            lower_name.ends_with(".d.ts")
                || lower_name.ends_with(".d.mts")
                || lower_name.ends_with(".d.cts")
        })
        .unwrap_or(false)
}

fn is_runtime_source_file(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();

    matches!(
        extension.as_str(),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts"
    )
}
