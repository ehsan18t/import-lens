use std::path::Path;

use crate::ipc::protocol::ImportDiagnostic;

/// Directories that never hold first-party package sources worth scanning:
/// dependency trees, VCS metadata, and build/coverage output. This list is
/// correctness-relevant - the approximate size walk and the types-only scan
/// must agree on what to skip - so it lives in exactly one place.
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

/// Whether `byte` may begin a JavaScript identifier (ASCII subset: the pipeline
/// only synthesizes and scans ASCII identifiers).
pub(crate) fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphabetic()
}

/// Whether `byte` may continue a JavaScript identifier.
pub(crate) fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

/// Builds a staged import diagnostic from an owned message and details.
pub(crate) fn diagnostic(stage: &str, message: String, details: Vec<String>) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: stage.to_owned(),
        message,
        details,
    }
}
