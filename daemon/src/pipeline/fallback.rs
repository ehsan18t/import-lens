use crate::ipc::protocol::ImportDiagnostic;
use crate::pipeline::util::should_skip_package_directory;
use std::{fs, path::Path};

const APPROXIMATE_MAX_FILES: usize = 10_000;
const APPROXIMATE_MAX_BYTES: u64 = 250 * 1024 * 1024;

pub(crate) fn approximate_directory_size(package_root: &Path) -> (u64, Vec<ImportDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut stack = vec![package_root.to_path_buf()];
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    let mut capped = false;

    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(ImportDiagnostic {
                    stage: "manifest_fallback".to_owned(),
                    message: format!(
                        "failed to read package directory during approximate sizing: {error}"
                    ),
                    details: vec![format!("directory: {}", directory.display())],
                });
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                if !should_skip_package_directory(&path) {
                    stack.push(path);
                }
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files += 1;
            bytes = bytes.saturating_add(metadata.len());
            if files >= APPROXIMATE_MAX_FILES || bytes >= APPROXIMATE_MAX_BYTES {
                capped = true;
                break;
            }
        }

        if capped {
            break;
        }
    }

    if capped {
        diagnostics.push(ImportDiagnostic {
            stage: "manifest_fallback".to_owned(),
            message: "approximate package directory traversal hit safety cap".to_owned(),
            details: vec![
                format!("max_files: {APPROXIMATE_MAX_FILES}"),
                format!("max_bytes: {APPROXIMATE_MAX_BYTES}"),
            ],
        });
    }

    (bytes, diagnostics)
}

pub(crate) fn estimate_minified_source(source: &str) -> String {
    let mut stripped = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = None;

    while let Some(c) = chars.next() {
        if let Some(quote) = in_string {
            stripped.push(c);
            if c == '\\' {
                if let Some(escaped) = chars.next() {
                    stripped.push(escaped);
                }
            } else if c == quote {
                in_string = None;
            }
        } else {
            match c {
                '\'' | '"' | '`' => {
                    in_string = Some(c);
                    stripped.push(c);
                }
                '/' => {
                    if let Some(&next) = chars.peek() {
                        if next == '/' {
                            chars.next();
                            for comment_char in chars.by_ref() {
                                if comment_char == '\n' {
                                    stripped.push('\n');
                                    break;
                                }
                            }
                        } else if next == '*' {
                            chars.next();
                            let mut prev_star = false;
                            for comment_char in chars.by_ref() {
                                if prev_star && comment_char == '/' {
                                    break;
                                }
                                prev_star = comment_char == '*';
                            }
                        } else {
                            stripped.push(c);
                        }
                    } else {
                        stripped.push(c);
                    }
                }
                _ => stripped.push(c),
            }
        }
    }

    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn source_excerpt_detail(source: &str) -> String {
    const MAX_EXCERPT_CHARS: usize = 240;
    let excerpt = source
        .chars()
        .take(MAX_EXCERPT_CHARS)
        .collect::<String>()
        .replace('\n', "\\n")
        .replace('\r', "\\r");

    if source.chars().count() > MAX_EXCERPT_CHARS {
        format!("source_excerpt: {excerpt}...")
    } else {
        format!("source_excerpt: {excerpt}")
    }
}
