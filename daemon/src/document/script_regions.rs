use crate::ipc::protocol::ImportRuntime;
use oxc_span::SourceType;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptLanguage {
    Js,
    Jsx,
    Ts,
    Tsx,
}

impl ScriptLanguage {
    fn extension(self) -> &'static str {
        match self {
            Self::Js => "js",
            Self::Jsx => "jsx",
            Self::Ts => "ts",
            Self::Tsx => "tsx",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScriptRegion<'a> {
    pub filename: String,
    pub source: &'a str,
    pub offset: usize,
    pub runtime: ImportRuntime,
}

pub fn script_regions_for_document<'a>(filename: &str, source: &'a str) -> Vec<ScriptRegion<'a>> {
    let lower_filename = filename.to_ascii_lowercase();

    if lower_filename.ends_with(".svelte") || lower_filename.ends_with(".vue") {
        return component_script_regions(filename, source);
    }

    if lower_filename.ends_with(".astro") {
        return astro_regions(filename, source);
    }

    vec![ScriptRegion {
        filename: filename.to_owned(),
        source,
        offset: 0,
        runtime: ImportRuntime::Component,
    }]
}

pub(super) fn source_type_for_region(filename: &str) -> SourceType {
    let source_type =
        SourceType::from_path(Path::new(filename)).unwrap_or_else(|_| SourceType::mjs());

    // JSX in plain .js is widespread (CRA-era apps, React Native). Enabling the
    // JSX variant only accepts a superset: a bare `<` can never start a valid
    // plain-JS expression, so no existing program changes meaning. TypeScript
    // stays untouched because `<T>x` assertions conflict with TSX.
    if source_type.is_javascript() {
        return source_type.with_jsx(true);
    }

    source_type
}

fn language_from_attributes(attributes: &str) -> ScriptLanguage {
    let lower_attributes = attributes.to_ascii_lowercase();
    let Some(lang_index) = lower_attributes.find("lang") else {
        return ScriptLanguage::Js;
    };
    let mut current = lang_index + "lang".len();
    current = skip_ascii_whitespace(&lower_attributes, current);

    if lower_attributes.as_bytes().get(current) != Some(&b'=') {
        return ScriptLanguage::Js;
    }

    current = skip_ascii_whitespace(&lower_attributes, current + 1);
    let Some(value) = read_attribute_value(&lower_attributes, current) else {
        return ScriptLanguage::Js;
    };

    match value.as_str() {
        "ts" | "typescript" => ScriptLanguage::Ts,
        "tsx" => ScriptLanguage::Tsx,
        "jsx" => ScriptLanguage::Jsx,
        _ => ScriptLanguage::Js,
    }
}

fn component_script_regions<'a>(filename: &str, source: &'a str) -> Vec<ScriptRegion<'a>> {
    script_blocks(source)
        .into_iter()
        .enumerate()
        .map(|(index, block)| {
            let language = language_from_attributes(block.attributes);
            ScriptRegion {
                filename: block_filename(filename, language, index),
                source: block.source,
                offset: block.content_start,
                runtime: ImportRuntime::Component,
            }
        })
        .collect()
}

fn astro_regions<'a>(filename: &str, source: &'a str) -> Vec<ScriptRegion<'a>> {
    let mut regions = Vec::new();

    if let Some(frontmatter) = astro_frontmatter(source) {
        regions.push(ScriptRegion {
            filename: block_filename(filename, ScriptLanguage::Ts, regions.len()),
            source: &source[frontmatter.source_start..frontmatter.source_end],
            offset: frontmatter.source_start,
            runtime: ImportRuntime::Server,
        });
    }

    for block in script_blocks(source) {
        if !is_processed_astro_script(block.attributes) {
            continue;
        }

        regions.push(ScriptRegion {
            filename: block_filename(filename, ScriptLanguage::Ts, regions.len()),
            source: block.source,
            offset: block.content_start,
            runtime: ImportRuntime::Client,
        });
    }

    regions
}

fn block_filename(filename: &str, language: ScriptLanguage, index: usize) -> String {
    format!("{filename}.{index}.{}", language.extension())
}

#[derive(Debug, Clone, Copy)]
struct ScriptBlock<'a> {
    attributes: &'a str,
    source: &'a str,
    content_start: usize,
}

fn script_blocks(source: &str) -> Vec<ScriptBlock<'_>> {
    let lower_source = source.to_ascii_lowercase();
    let mut blocks = Vec::new();
    let mut search_offset = 0;

    while let Some(relative_start) = lower_source[search_offset..].find("<script") {
        let tag_start = search_offset + relative_start;
        let after_name = tag_start + "<script".len();
        if !is_tag_boundary(lower_source.as_bytes().get(after_name).copied()) {
            search_offset = after_name;
            continue;
        }

        let Some(relative_tag_end) = lower_source[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + relative_tag_end;
        let content_start = tag_end + 1;
        let Some(relative_end) = lower_source[content_start..].find("</script>") else {
            break;
        };
        let content_end = content_start + relative_end;

        blocks.push(ScriptBlock {
            attributes: &source[after_name..tag_end],
            source: &source[content_start..content_end],
            content_start,
        });
        search_offset = content_end + "</script>".len();
    }

    blocks
}

fn is_tag_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| byte == b'>' || byte.is_ascii_whitespace() || byte == b'/')
}

#[derive(Debug, Clone, Copy)]
struct Frontmatter {
    source_start: usize,
    source_end: usize,
}

fn astro_frontmatter(source: &str) -> Option<Frontmatter> {
    if !source.starts_with("---") {
        return None;
    }

    let opening_newline = line_ending_after(source, 3)?;
    let content_start = opening_newline;
    let mut line_start = content_start;

    while line_start < source.len() {
        let line_end = next_line_end(source, line_start);
        if source[line_start..line_end].trim_end_matches('\r') == "---" {
            let content_end = previous_line_end(source, line_start);
            return Some(Frontmatter {
                source_start: content_start,
                source_end: content_end,
            });
        }

        line_start = line_ending_after(source, line_end)?;
    }

    None
}

fn line_ending_after(source: &str, offset: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if offset >= bytes.len() {
        return None;
    }

    match bytes[offset] {
        b'\r' if bytes.get(offset + 1) == Some(&b'\n') => Some(offset + 2),
        b'\r' | b'\n' => Some(offset + 1),
        _ => None,
    }
}

fn next_line_end(source: &str, offset: usize) -> usize {
    source[offset..]
        .find(['\r', '\n'])
        .map_or(source.len(), |relative| offset + relative)
}

fn previous_line_end(source: &str, offset: usize) -> usize {
    if offset > 0 && source.as_bytes().get(offset - 1) == Some(&b'\n') {
        if offset > 1 && source.as_bytes().get(offset - 2) == Some(&b'\r') {
            return offset - 2;
        }

        return offset - 1;
    }

    if offset > 0 && source.as_bytes().get(offset - 1) == Some(&b'\r') {
        return offset - 1;
    }

    offset
}

fn is_processed_astro_script(attributes: &str) -> bool {
    let normalized = attributes.trim();

    if normalized.is_empty() {
        return true;
    }

    let lower = normalized.to_ascii_lowercase();
    if !lower.starts_with("src") {
        return false;
    }

    let mut current = skip_ascii_whitespace(&lower, "src".len());
    if lower.as_bytes().get(current) != Some(&b'=') {
        return false;
    }

    current = skip_ascii_whitespace(&lower, current + 1);
    let Some((_, end)) = read_attribute_value_with_end(&lower, current) else {
        return false;
    };

    lower[end..].trim().is_empty()
}

fn skip_ascii_whitespace(value: &str, mut offset: usize) -> usize {
    while value
        .as_bytes()
        .get(offset)
        .is_some_and(u8::is_ascii_whitespace)
    {
        offset += 1;
    }

    offset
}

fn read_attribute_value(value: &str, offset: usize) -> Option<String> {
    read_attribute_value_with_end(value, offset).map(|(value, _)| value)
}

fn read_attribute_value_with_end(value: &str, offset: usize) -> Option<(String, usize)> {
    let byte = *value.as_bytes().get(offset)?;
    if byte == b'"' || byte == b'\'' {
        let quote = byte;
        let start = offset + 1;
        let relative_end = value[start..].find(quote as char)?;
        let end = start + relative_end;
        return Some((value[start..end].to_owned(), end + 1));
    }

    let end = value[offset..]
        .find(|char: char| char.is_ascii_whitespace() || char == '>')
        .map_or(value.len(), |relative| offset + relative);
    Some((value[offset..end].to_owned(), end))
}
