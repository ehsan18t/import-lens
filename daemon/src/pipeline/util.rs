use std::path::Path;

use crate::ipc::protocol::ImportDiagnostic;

/// Directories that never hold package sources worth scanning: dependency trees, VCS
/// metadata, and build/coverage output. The one list shared by the types-only scan and the
/// workspace report scanner.
pub(crate) fn should_skip_package_directory(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        name,
        "node_modules"
            | ".git"
            | ".hg"
            | ".svn"
            | ".cache"
            | ".turbo"
            | ".parcel-cache"
            | ".next"
            | ".nuxt"
            | ".vite"
            | "coverage"
            | "target"
    )
}

/// Builds a staged import diagnostic from an owned message and details.
pub(crate) fn diagnostic(stage: &str, message: String, details: Vec<String>) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: stage.to_owned(),
        message,
        details,
    }
}
