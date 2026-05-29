use crate::{
    ipc::protocol::{ImportDiagnostic, ImportKind, ImportRequest, ImportResult},
    pipeline::{compress::compress_all, resolver::resolve_package_entry},
};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone)]
pub struct AnalysisContext {
    pub workspace_root: PathBuf,
    pub active_document_path: PathBuf,
}

#[derive(Debug, Clone)]
struct AnalysisError {
    stage: &'static str,
    message: String,
    details: Vec<String>,
}

pub fn analyze_import(context: &AnalysisContext, request: &ImportRequest) -> ImportResult {
    match analyze_import_inner(context, request) {
        Ok(result) => result,
        Err(error) => error_result(request, error),
    }
}

fn analyze_import_inner(
    context: &AnalysisContext,
    request: &ImportRequest,
) -> Result<ImportResult, AnalysisError> {
    let resolved =
        resolve_package_entry(&context.active_document_path, request).map_err(|message| {
            let stage = if message.contains("unsafe package name") {
                "package_validation"
            } else if message.contains("package manifest not found") {
                "package_resolution"
            } else {
                "entry_resolution"
            };
            let details = resolver_details(&message);
            error_with_context(stage, message, context, request, details)
        })?;
    let side_effects = resolved.side_effects;
    let entry_path = resolved.entry_path;
    let is_cjs = resolved.is_cjs;

    let metadata = fs::metadata(&entry_path).map_err(|error| {
        error_with_context(
            "entry_metadata",
            format!(
                "failed to stat package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;

    let max_size = 5 * 1024 * 1024;
    if metadata.len() > max_size {
        return Err(error_with_context(
            "file_size_limit",
            format!("file size {} exceeds 5MB limit", metadata.len()),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        ));
    }

    let source = fs::read_to_string(&entry_path).map_err(|error| {
        error_with_context(
            "entry_read",
            format!(
                "failed to read package entry {}: {error}",
                entry_path.display()
            ),
            context,
            request,
            vec![format!("entry_path: {}", entry_path.display())],
        )
    })?;
    let minified = estimate_minified_source(&source);
    let compressed = compress_all(&minified).map_err(|error| {
        error_with_context(
            "compression",
            format!("failed to compress minified output: {error}"),
            context,
            request,
            Vec::new(),
        )
    })?;
    let raw_bytes = source.len() as u64;
    let minified_bytes = minified.len() as u64;

    Ok(ImportResult {
        specifier: request.specifier.clone(),
        raw_bytes,
        minified_bytes,
        gzip_bytes: compressed.gzip_bytes,
        brotli_bytes: compressed.brotli_bytes,
        zstd_bytes: compressed.zstd_bytes,
        cache_hit: false,
        side_effects,
        truly_treeshakeable: !side_effects
            && !is_cjs
            && matches!(request.import_kind, ImportKind::Named),
        is_cjs,
        error: None,
        diagnostics: Vec::new(),
    })
}

fn error_result(request: &ImportRequest, error: AnalysisError) -> ImportResult {
    ImportResult {
        specifier: request.specifier.clone(),
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        cache_hit: false,
        side_effects: true,
        truly_treeshakeable: false,
        is_cjs: false,
        error: Some(error.message.clone()),
        diagnostics: vec![ImportDiagnostic {
            stage: error.stage.to_owned(),
            message: error.message,
            details: error.details,
        }],
    }
}

fn resolver_details(message: &str) -> Vec<String> {
    message
        .split("; ")
        .filter(|part| part.starts_with("checked:") || part.starts_with("candidate:"))
        .map(str::to_owned)
        .collect()
}

fn estimate_minified_source(source: &str) -> String {
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

fn error_with_context(
    stage: &'static str,
    message: impl Into<String>,
    context: &AnalysisContext,
    request: &ImportRequest,
    details: Vec<String>,
) -> AnalysisError {
    let mut context_details = vec![
        format!("specifier: {}", request.specifier),
        format!("package: {}", request.package_name),
        format!(
            "active_document_path: {}",
            context.active_document_path.display()
        ),
        format!("workspace_root: {}", context.workspace_root.display()),
    ];
    context_details.extend(details);

    AnalysisError {
        stage,
        message: message.into(),
        details: context_details,
    }
}
