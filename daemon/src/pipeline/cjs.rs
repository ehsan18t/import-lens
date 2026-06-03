use crate::{
    ipc::protocol::{ImportDiagnostic, ImportRuntime, ModuleContribution},
    pipeline::{
        graph::{MAX_GRAPH_MODULES, MAX_GRAPH_SOURCE_BYTES, MAX_MODULE_SOURCE_BYTES},
        resolver::{create_resolver, normalize_existing_path, resolve_module_path},
    },
};
use oxc_resolver::Resolver;
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct CjsGraphAnalysis {
    pub source: String,
    pub module_breakdown: Vec<ModuleContribution>,
    pub full_module_breakdown: Vec<ModuleContribution>,
    pub exports: Vec<String>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub unsupported: bool,
}

pub fn analyze_cjs_graph(entry_path: &Path) -> Result<CjsGraphAnalysis, String> {
    analyze_cjs_graph_with_runtime(entry_path, ImportRuntime::Component)
}

pub fn analyze_cjs_graph_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<CjsGraphAnalysis, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    let mut queue = VecDeque::from([entry_path]);
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    let mut module_breakdown = Vec::new();
    let mut exports = Vec::new();
    let mut diagnostics = Vec::new();
    let mut total_source_bytes = 0_usize;
    let mut unsupported = false;
    let resolver = create_resolver(runtime);

    while let Some(path) = queue.pop_front() {
        let path = normalize_existing_path(&path)?;
        if !seen.insert(path.clone()) {
            continue;
        }
        if seen.len() > MAX_GRAPH_MODULES {
            return Err(format!(
                "CommonJS module count limit exceeded while loading {}; limit: {}",
                path.display(),
                MAX_GRAPH_MODULES
            ));
        }

        let source = fs::read_to_string(&path).map_err(|error| {
            format!("failed to read CommonJS module {}: {error}", path.display())
        })?;
        let source_bytes = source.len();
        if source_bytes > MAX_MODULE_SOURCE_BYTES {
            return Err(format!(
                "CommonJS module source size {} exceeds limit {} in {}",
                source_bytes,
                MAX_MODULE_SOURCE_BYTES,
                path.display()
            ));
        }
        total_source_bytes = total_source_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| format!("CommonJS graph source size overflow in {}", path.display()))?;
        if total_source_bytes > MAX_GRAPH_SOURCE_BYTES {
            return Err(format!(
                "CommonJS graph source size {} exceeds limit {} while loading {}",
                total_source_bytes,
                MAX_GRAPH_SOURCE_BYTES,
                path.display()
            ));
        }

        let masked = mask_non_code(&source);
        let requires = literal_requires(&source, &masked, &mut unsupported);
        for specifier in requires {
            match resolve_require(&resolver, &path, &specifier) {
                Ok(Some(resolved_path)) => queue.push_back(resolved_path),
                Ok(None) => diagnostics.push(diagnostic(
                    "cjs_resolution",
                    format!("CommonJS require '{specifier}' was kept external"),
                    vec![format!("from_path: {}", path.display())],
                )),
                Err(error) => {
                    diagnostics.push(diagnostic(
                        "cjs_resolution",
                        error,
                        vec![format!("from_path: {}", path.display())],
                    ));
                    unsupported = true;
                }
            }
        }

        if seen.len() == 1 {
            exports.extend(commonjs_exports(&source, &masked));
        }

        module_breakdown.push(ModuleContribution {
            path: path.to_string_lossy().to_string(),
            bytes: source_bytes as u64,
        });
        sources.push(format!(";(() => {{\n{source}\n}})();"));
    }

    exports.sort();
    exports.dedup();
    module_breakdown.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    let full_module_breakdown = module_breakdown.clone();
    module_breakdown.truncate(10);

    Ok(CjsGraphAnalysis {
        source: sources.join("\n"),
        module_breakdown,
        full_module_breakdown,
        exports,
        diagnostics,
        unsupported,
    })
}

fn literal_requires(source: &str, masked: &str, unsupported: &mut bool) -> Vec<String> {
    let bytes = masked.as_bytes();
    let mut specifiers = Vec::new();
    let mut index = 0;

    while let Some(relative) = masked[index..].find("require") {
        let start = index + relative;
        let end = start + "require".len();
        if !is_boundary(bytes, start, end) {
            index = end;
            continue;
        }

        let open = skip_spaces(bytes, end);
        if bytes.get(open) != Some(&b'(') {
            index = end;
            continue;
        }

        let argument = skip_spaces(bytes, open + 1);
        match parse_string_literal(source.as_bytes(), argument) {
            Some((specifier, _)) => specifiers.push(specifier),
            None => *unsupported = true,
        }
        index = open + 1;
    }

    specifiers
}

fn commonjs_exports(source: &str, masked: &str) -> Vec<String> {
    let bytes = masked.as_bytes();
    let mut exports = Vec::new();
    let mut index = 0;

    while let Some(relative) = masked[index..].find("exports") {
        let start = index + relative;
        let end = start + "exports".len();
        if !is_identifier_boundary(bytes, start, end) {
            index = end;
            continue;
        }

        if let Some((name, next_index)) = export_name_after_exports(bytes, end) {
            exports.push(name);
            index = next_index;
            continue;
        }

        if is_module_exports(bytes, start) {
            if let Some((name, next_index)) = export_name_after_exports(bytes, end) {
                exports.push(name);
                index = next_index;
                continue;
            }
            if let Some((names, next_index)) = object_export_names(source, bytes, end) {
                exports.extend(names);
                index = next_index;
                continue;
            }
            if is_default_module_export(bytes, end) {
                exports.push("default".to_owned());
            }
        }

        index = end;
    }

    exports
}

fn export_name_after_exports(bytes: &[u8], exports_end: usize) -> Option<(String, usize)> {
    let dot = skip_spaces(bytes, exports_end);
    if bytes.get(dot) != Some(&b'.') {
        return None;
    }
    let name_start = skip_spaces(bytes, dot + 1);
    let (name, name_end) = read_identifier(bytes, name_start)?;
    let assignment = skip_spaces(bytes, name_end);
    if bytes.get(assignment) != Some(&b'=') {
        return None;
    }
    Some((name, assignment + 1))
}

fn object_export_names(
    source: &str,
    bytes: &[u8],
    exports_end: usize,
) -> Option<(Vec<String>, usize)> {
    let assignment = skip_spaces(bytes, exports_end);
    if bytes.get(assignment) != Some(&b'=') {
        return None;
    }
    let object_start = skip_spaces(bytes, assignment + 1);
    if bytes.get(object_start) != Some(&b'{') {
        return None;
    }

    let object_end = find_matching_brace(bytes, object_start)?;
    let object_source = source.get(object_start + 1..object_end)?;
    let names = object_source
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            let name = part.split(':').next()?.trim();
            if name
                .chars()
                .all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
            {
                Some(name.to_owned())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    Some((names, object_end + 1))
}

fn is_default_module_export(bytes: &[u8], exports_end: usize) -> bool {
    let assignment = skip_spaces(bytes, exports_end);
    if bytes.get(assignment) != Some(&b'=') {
        return false;
    }
    let value_start = skip_spaces(bytes, assignment + 1);
    bytes.get(value_start..).is_some_and(|remaining| {
        remaining.starts_with(b"function") || remaining.starts_with(b"class")
    })
}

fn is_module_exports(bytes: &[u8], exports_start: usize) -> bool {
    let Some(module_start) = exports_start.checked_sub("module.".len()) else {
        return false;
    };
    bytes
        .get(module_start..exports_start)
        .is_some_and(|prefix| prefix == b"module.")
        && is_identifier_boundary(bytes, module_start, exports_start + "exports".len())
}

fn resolve_require(
    resolver: &Resolver,
    from_path: &Path,
    specifier: &str,
) -> Result<Option<PathBuf>, String> {
    if !specifier.starts_with('.') {
        return Ok(None);
    }

    let from_dir = from_path.parent().ok_or_else(|| {
        format!(
            "CommonJS module path has no parent directory: {}",
            from_path.display()
        )
    })?;
    resolve_module_path(resolver, from_dir, specifier)
        .map(|resolved| Some(resolved.path))
        .map_err(|error| {
            format!(
                "failed to resolve CommonJS require '{specifier}' from {}: {error}",
                from_path.display()
            )
        })
}

fn mask_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[index];
                index += 1;
                while index < bytes.len() {
                    let current = bytes[index];
                    masked[index] = if current == b'\n' { b'\n' } else { b' ' };
                    index += 1;
                    if current == b'\\' {
                        if index < bytes.len() {
                            masked[index] = b' ';
                            index += 1;
                        }
                    } else if current == quote {
                        break;
                    }
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                masked[index] = b' ';
                masked[index + 1] = b' ';
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    masked[index] = b' ';
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                masked[index] = b' ';
                masked[index + 1] = b' ';
                index += 2;
                while index < bytes.len() {
                    let current = bytes[index];
                    let next = bytes.get(index + 1).copied();
                    masked[index] = if current == b'\n' { b'\n' } else { b' ' };
                    index += 1;
                    if current == b'*' && next == Some(b'/') {
                        masked[index] = b' ';
                        index += 1;
                        break;
                    }
                }
            }
            _ => index += 1,
        }
    }

    String::from_utf8(masked).expect("masked source should remain valid UTF-8")
}

fn parse_string_literal(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let quote = *bytes.get(start)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }

    let mut value = String::new();
    let mut index = start + 1;
    while index < bytes.len() {
        let current = bytes[index];
        if current == quote {
            return Some((value, index + 1));
        }
        if current == b'\\' {
            index += 1;
            value.push(*bytes.get(index)? as char);
        } else {
            value.push(current as char);
        }
        index += 1;
    }

    None
}

fn find_matching_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn skip_spaces(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn read_identifier(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let first = *bytes.get(start)?;
    if !is_identifier_start(first) {
        return None;
    }

    let mut end = start + 1;
    while bytes.get(end).is_some_and(|byte| is_identifier_part(*byte)) {
        end += 1;
    }
    Some((String::from_utf8_lossy(&bytes[start..end]).to_string(), end))
}

fn is_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    is_identifier_boundary(bytes, start, end)
}

fn is_identifier_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_none_or(|byte| !is_identifier_part(*byte))
        && bytes.get(end).is_none_or(|byte| !is_identifier_part(*byte))
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphabetic()
}

fn is_identifier_part(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn diagnostic(stage: &str, message: String, details: Vec<String>) -> ImportDiagnostic {
    ImportDiagnostic {
        stage: stage.to_owned(),
        message,
        details,
    }
}
