#[derive(Debug, Default)]
pub(crate) struct CjsSourceScan {
    pub(crate) requires: Vec<String>,
    pub(crate) exports: Vec<String>,
    pub(crate) unsupported: bool,
}

pub(crate) fn scan_cjs_source(source: &str) -> CjsSourceScan {
    let masked = mask_non_code(source);
    let mut unsupported = false;
    let requires = literal_requires(source, &masked, &mut unsupported);
    let exports = commonjs_exports(source, &masked);

    CjsSourceScan {
        requires,
        exports,
        unsupported,
    }
}

fn literal_requires(source: &str, masked: &str, unsupported: &mut bool) -> Vec<String> {
    let bytes = masked.as_bytes();
    let mut specifiers = Vec::new();
    let mut index = 0;

    while let Some(start) = find_ascii_token(bytes, b"require", index) {
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

    while let Some(start) = find_ascii_token(bytes, b"exports", index) {
        let end = start + "exports".len();
        if !is_identifier_boundary(bytes, start, end) {
            index = end;
            continue;
        }

        if let Some((name, next_index)) = export_name_after_exports(source.as_bytes(), bytes, end) {
            exports.push(name);
            index = next_index;
            continue;
        }

        if is_module_exports(bytes, start) {
            if let Some((name, next_index)) =
                export_name_after_exports(source.as_bytes(), bytes, end)
            {
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

fn export_name_after_exports(
    source_bytes: &[u8],
    bytes: &[u8],
    exports_end: usize,
) -> Option<(String, usize)> {
    let property_start = skip_spaces(bytes, exports_end);
    let (name, name_end) = if bytes.get(property_start) == Some(&b'.') {
        let name_start = skip_spaces(bytes, property_start + 1);
        read_identifier(bytes, name_start)?
    } else if bytes.get(property_start) == Some(&b'[') {
        let string_start = skip_spaces(bytes, property_start + 1);
        let (name, string_end) = parse_string_literal(source_bytes, string_start)?;
        let bracket_end = skip_spaces(bytes, string_end);
        if bytes.get(bracket_end) != Some(&b']') {
            return None;
        }
        (name, bracket_end + 1)
    } else {
        return None;
    };
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

fn mask_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => index = mask_string_literal(bytes, &mut masked, index),
            b'`' => index = mask_template_literal(bytes, &mut masked, index),
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
            b'/' if is_regex_literal_start(bytes, index) => {
                index = mask_regex_literal(bytes, &mut masked, index);
            }
            _ => index += 1,
        }
    }

    String::from_utf8(masked).expect("masked source should remain valid UTF-8")
}

fn mask_string_literal(bytes: &[u8], masked: &mut [u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;

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

    index
}

fn mask_template_literal(bytes: &[u8], masked: &mut [u8], start: usize) -> usize {
    let mut index = start + 1;

    while index < bytes.len() {
        let current = bytes[index];
        masked[index] = if current == b'\n' { b'\n' } else { b' ' };

        if current == b'\\' {
            index += 1;
            if index < bytes.len() {
                masked[index] = if bytes[index] == b'\n' { b'\n' } else { b' ' };
            }
        } else if current == b'`' {
            index += 1;
            break;
        } else if current == b'$' && bytes.get(index + 1) == Some(&b'{') {
            masked[index + 1] = b' ';
            index = mask_template_expression(bytes, masked, index + 2);
            continue;
        }

        index += 1;
    }

    index
}

fn mask_template_expression(bytes: &[u8], masked: &mut [u8], start: usize) -> usize {
    let mut index = start;
    let mut depth = 1_usize;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => index = mask_string_literal(bytes, masked, index),
            b'`' => index = mask_template_literal(bytes, masked, index),
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
            b'/' if is_regex_literal_start(bytes, index) => {
                index = mask_regex_literal(bytes, masked, index);
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    masked[index] = b' ';
                    return index + 1;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }

    index
}

fn is_regex_literal_start(bytes: &[u8], start: usize) -> bool {
    let Some(previous) = previous_significant_byte(bytes, start) else {
        return true;
    };

    matches!(
        previous,
        b'(' | b'=' | b':' | b'[' | b'{' | b',' | b'!' | b'?' | b';'
    )
}

fn previous_significant_byte(bytes: &[u8], start: usize) -> Option<u8> {
    bytes[..start]
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn mask_regex_literal(bytes: &[u8], masked: &mut [u8], start: usize) -> usize {
    masked[start] = b' ';
    let mut index = start + 1;
    let mut in_character_class = false;

    while index < bytes.len() {
        let current = bytes[index];
        masked[index] = if current == b'\n' { b'\n' } else { b' ' };
        index += 1;

        if current == b'\\' {
            if index < bytes.len() {
                masked[index] = if bytes[index] == b'\n' { b'\n' } else { b' ' };
                index += 1;
            }
            continue;
        }

        match current {
            b'[' => in_character_class = true,
            b']' => in_character_class = false,
            b'/' if !in_character_class => {
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphabetic())
                {
                    masked[index] = b' ';
                    index += 1;
                }
                break;
            }
            _ => {}
        }
    }

    index
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

fn find_ascii_token(bytes: &[u8], token: &[u8], mut index: usize) -> Option<usize> {
    if token.is_empty() || token.len() > bytes.len() {
        return None;
    }

    while index <= bytes.len() - token.len() {
        if bytes[index] == token[0] && &bytes[index..index + token.len()] == token {
            return Some(index);
        }

        index += 1;
    }

    None
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
