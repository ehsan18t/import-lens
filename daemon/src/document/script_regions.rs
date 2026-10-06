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
    /// The document's markup can reference this region's top-level bindings: a Vue or Svelte
    /// component script, or Astro frontmatter. The region is then not the whole module, so a
    /// binding its script uses only as a type is still a runtime value when the markup uses it.
    pub shares_bindings_with_markup: bool,
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
        shares_bindings_with_markup: false,
    }]
}

/// The places in a component's markup its compiler treats as references to script bindings:
/// Vue `{{ }}` interpolations, bound and directive attribute values (`:x`, `@x`, `#x`, `v-x`) and
/// component tags; Svelte and Astro `{ }` expressions and component tags. Static text, comments,
/// `<style>` and raw `<script>` bodies are not references, so a type-only name that merely appears
/// there stays elided. Empty when no region shares its bindings with markup.
pub(super) fn template_references<'a>(
    filename: &str,
    source: &'a str,
    regions: &[ScriptRegion<'_>],
) -> Vec<&'a str> {
    if !regions
        .iter()
        .any(|region| region.shares_bindings_with_markup)
    {
        return Vec::new();
    }
    let syntax = if filename.to_ascii_lowercase().ends_with(".vue") {
        TemplateSyntax::Vue
    } else {
        TemplateSyntax::Braces
    };

    let mut spans: Vec<(usize, usize)> = regions
        .iter()
        .map(|region| (region.offset, region.offset + region.source.len()))
        .collect();
    spans.sort_unstable();

    let mut references = Vec::new();
    let mut cursor = 0;
    for (start, end) in spans {
        if start > cursor {
            scan_template(&source[cursor..start], syntax, &mut references);
        }
        cursor = cursor.max(end);
    }
    scan_template(&source[cursor..], syntax, &mut references);
    references
}

/// Whether a template reference names `name`, or a multi-word PascalCase `name` in the kebab-case
/// tag spelling Vue also resolves (`NButton` as `<n-button>`).
pub(super) fn markup_references(references: &[&str], name: &str) -> bool {
    let kebab = kebab_case(name);
    references.iter().any(|text| {
        contains_identifier(text, name)
            || kebab
                .as_deref()
                .is_some_and(|kebab| contains_identifier(text, kebab))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateSyntax {
    /// `{{ expr }}` in text; expressions only in bound or directive attribute values.
    Vue,
    /// `{expr}` in text and attributes (Svelte, Astro).
    Braces,
}

/// Collects the reference-bearing slices of one markup fragment. Byte-indexed; every slice starts
/// and ends at an ASCII delimiter, so it is always on a char boundary.
fn scan_template<'a>(markup: &'a str, syntax: TemplateSyntax, references: &mut Vec<&'a str>) {
    let bytes = markup.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        let rest = &bytes[index..];
        if rest.starts_with(b"<!--") {
            index = find_bytes(bytes, index + 4, b"-->").map_or(bytes.len(), |end| end + 3);
        } else if rest.starts_with(b"</") {
            index = find_bytes(bytes, index, b">").map_or(bytes.len(), |end| end + 1);
        } else if rest[0] == b'<' && rest.get(1).is_some_and(u8::is_ascii_alphabetic) {
            index = scan_tag(markup, index + 1, syntax, references);
        } else if let Some(end) = expression_end(bytes, index, syntax) {
            references.push(&markup[index..end]);
            index = end;
        } else {
            index += 1;
        }
    }
}

/// Scans one open tag whose name starts at `name_start` and returns the index after it, or after
/// the body of a raw-text element (`<style>`, an unprocessed `<script>`), which is never markup.
fn scan_tag<'a>(
    markup: &'a str,
    name_start: usize,
    syntax: TemplateSyntax,
    references: &mut Vec<&'a str>,
) -> usize {
    let bytes = markup.as_bytes();
    let name_end = until(bytes, name_start, |byte| {
        byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')
    });
    let name = &markup[name_start..name_end];
    // A lowercase single-word tag is a native element in all three compilers; only these can name
    // a component (`Card`, `n-card`, `Foo.Bar`).
    if name
        .bytes()
        .any(|byte| byte.is_ascii_uppercase() || matches!(byte, b'-' | b'.'))
    {
        references.push(name);
    }

    let mut index = name_end;
    loop {
        index = skip_ascii_whitespace(markup, index);
        match bytes.get(index) {
            None => return bytes.len(),
            Some(b'>') => break,
            Some(b'{') if syntax == TemplateSyntax::Braces => {
                let end = expression_end(bytes, index, syntax).unwrap_or(bytes.len());
                references.push(&markup[index..end]);
                index = end;
            }
            Some(_) => {
                let attribute_end = until(bytes, index, |byte| {
                    byte.is_ascii_whitespace() || matches!(byte, b'=' | b'>')
                })
                .max(index + 1);
                let attribute = &markup[index..attribute_end];
                index = skip_ascii_whitespace(markup, attribute_end);
                if bytes.get(index) != Some(&b'=') {
                    continue;
                }
                index = skip_ascii_whitespace(markup, index + 1);
                let (value, next) = attribute_value_at(markup, index, syntax);
                let is_expression = match syntax {
                    TemplateSyntax::Vue => {
                        attribute.starts_with([':', '@', '#']) || attribute.starts_with("v-")
                    }
                    TemplateSyntax::Braces => value.contains('{'),
                };
                if is_expression {
                    references.push(value);
                }
                index = next;
            }
        }
    }

    let after_tag = index + 1;
    if name.eq_ignore_ascii_case("style") || name.eq_ignore_ascii_case("script") {
        let close = format!("</{}", name.to_ascii_lowercase());
        return markup[after_tag..]
            .to_ascii_lowercase()
            .find(&close)
            .map_or(bytes.len(), |relative| after_tag + relative);
    }
    after_tag
}

/// An attribute value starting at `index` (quoted, a `{ }` expression, or bare) and the index
/// after it.
fn attribute_value_at(markup: &str, index: usize, syntax: TemplateSyntax) -> (&str, usize) {
    let bytes = markup.as_bytes();
    match bytes.get(index) {
        Some(&quote @ (b'"' | b'\'')) => {
            let end = find_bytes(bytes, index + 1, &[quote]).unwrap_or(bytes.len());
            (&markup[index + 1..end], (end + 1).min(bytes.len()))
        }
        Some(b'{') if syntax == TemplateSyntax::Braces => {
            let end = expression_end(bytes, index, syntax).unwrap_or(bytes.len());
            (&markup[index..end], end)
        }
        _ => {
            let end = until(bytes, index, |byte| {
                byte.is_ascii_whitespace() || byte == b'>'
            });
            (&markup[index..end], end)
        }
    }
}

/// The end of the interpolation starting at `index`, if one starts there: `{{ ... }}` for Vue, a
/// brace-balanced `{ ... }` otherwise.
fn expression_end(bytes: &[u8], index: usize, syntax: TemplateSyntax) -> Option<usize> {
    match syntax {
        TemplateSyntax::Vue => bytes[index..]
            .starts_with(b"{{")
            .then(|| find_bytes(bytes, index + 2, b"}}").map_or(bytes.len(), |end| end + 2)),
        TemplateSyntax::Braces => (bytes[index] == b'{').then(|| {
            let mut depth = 0usize;
            for (offset, byte) in bytes[index..].iter().enumerate() {
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return index + offset + 1;
                        }
                    }
                    _ => {}
                }
            }
            bytes.len()
        }),
    }
}

fn until(bytes: &[u8], from: usize, stop: impl Fn(u8) -> bool) -> usize {
    bytes[from..]
        .iter()
        .position(|&byte| stop(byte))
        .map_or(bytes.len(), |relative| from + relative)
}

fn find_bytes(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|relative| from + relative)
}

fn contains_identifier(text: &str, word: &str) -> bool {
    let is_identifier_char = |char: char| char.is_alphanumeric() || char == '_' || char == '$';
    text.match_indices(word).any(|(start, _)| {
        !text[..start]
            .chars()
            .next_back()
            .is_some_and(is_identifier_char)
            && !text[start + word.len()..]
                .chars()
                .next()
                .is_some_and(is_identifier_char)
    })
}

/// `NButton` -> `n-button`; `None` for a name with no inner capital, whose kebab spelling would be
/// a plain lowercase word and match ordinary template text.
fn kebab_case(name: &str) -> Option<String> {
    let mut kebab = String::with_capacity(name.len() + 4);
    let mut hyphenated = false;
    for (index, char) in name.char_indices() {
        if index > 0 && char.is_ascii_uppercase() {
            kebab.push('-');
            hyphenated = true;
        }
        kebab.push(char.to_ascii_lowercase());
    }
    hyphenated.then_some(kebab)
}

/// The import runtime in effect at a document cursor, from the one document classifier.
///
/// This is the sole authority for "what conditions does an import here resolve under?"
/// (ADR-0002): completion and export enumeration both go through it, so a name offered
/// and the size measured for that same import can never disagree. A cursor outside every
/// runtime-bearing region — a plain `.ts`/`.js`/`.jsx` file, or the HTML body of an
/// `.astro`/`.vue`/`.svelte` document — is `Component`, the default a bare file already
/// carries.
pub fn runtime_at_offset(
    filename: &str,
    source: &str,
    utf16_cursor_offset: usize,
) -> ImportRuntime {
    let offset = byte_offset_for_utf16(source, utf16_cursor_offset);

    for region in script_regions_for_document(filename, source) {
        let region_end = region.offset + region.source.len();
        if offset >= region.offset && offset <= region_end {
            return region.runtime;
        }
    }

    ImportRuntime::Component
}

/// VS Code's `document.offsetAt` counts UTF-16 code units, while oxc spans and the region
/// offsets above are byte offsets; the two only coincide for pure-ASCII prefixes.
pub(super) fn byte_offset_for_utf16(source: &str, utf16_offset: usize) -> usize {
    let mut utf16_seen = 0;

    for (byte_index, char) in source.char_indices() {
        if utf16_seen >= utf16_offset {
            return byte_index;
        }
        utf16_seen += char.len_utf16();
    }

    source.len()
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
    match attribute_value(attributes, "lang").as_deref() {
        Some("ts" | "typescript") => ScriptLanguage::Ts,
        Some("tsx") => ScriptLanguage::Tsx,
        Some("jsx") => ScriptLanguage::Jsx,
        _ => ScriptLanguage::Js,
    }
}

/// The lowercased value of the attribute named exactly `wanted`: `Some("")` for a valueless one
/// (`setup`), `None` when absent. Walks `name[=value]` tokens, so `data-slang="x"` is not `lang`.
fn attribute_value(attributes: &str, wanted: &str) -> Option<String> {
    let lower = attributes.to_ascii_lowercase();
    let mut offset = skip_ascii_whitespace(&lower, 0);

    while offset < lower.len() {
        let name_end = lower[offset..]
            .find(|char: char| {
                char.is_ascii_whitespace() || char == '=' || char == '/' || char == '>'
            })
            .map_or(lower.len(), |relative| offset + relative);

        if name_end == offset {
            offset = skip_ascii_whitespace(&lower, offset + 1);
            continue;
        }

        let name = &lower[offset..name_end];
        let after_name = skip_ascii_whitespace(&lower, name_end);

        if lower.as_bytes().get(after_name) == Some(&b'=') {
            let value_start = skip_ascii_whitespace(&lower, after_name + 1);
            let (value, value_end) = read_attribute_value_with_end(&lower, value_start)
                .unwrap_or((String::new(), value_start + 1));
            if name == wanted {
                return Some(value);
            }
            offset = skip_ascii_whitespace(&lower, value_end);
        } else {
            if name == wanted {
                return Some(String::new());
            }
            offset = after_name;
        }
    }

    None
}

/// Every Svelte script's top-level bindings reach the markup. In Vue only `<script setup>` does,
/// together with a plain `<script>` beside it; an Options API template sees only what the component
/// object registers, never the script's imports.
fn component_script_regions<'a>(filename: &str, source: &'a str) -> Vec<ScriptRegion<'a>> {
    let blocks = script_blocks(source);
    let template_sees_imports = !filename.to_ascii_lowercase().ends_with(".vue")
        || blocks
            .iter()
            .any(|block| attribute_value(block.attributes, "setup").is_some());

    blocks
        .into_iter()
        .enumerate()
        .map(|(index, block)| {
            let language = language_from_attributes(block.attributes);
            ScriptRegion {
                filename: block_filename(filename, language, index),
                source: block.source,
                offset: block.content_start,
                runtime: ImportRuntime::Component,
                shares_bindings_with_markup: template_sees_imports,
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
            shares_bindings_with_markup: true,
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
            shares_bindings_with_markup: false,
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
        // A `<script` inside an HTML comment is not a tag. An unterminated comment runs to the
        // end of the document, as it does in HTML.
        if let Some(comment_start) = lower_source[search_offset..tag_start].find("<!--") {
            let after_open = search_offset + comment_start + "<!--".len();
            let Some(comment_end) = lower_source[after_open..].find("-->") else {
                break;
            };
            search_offset = after_open + comment_end + "-->".len();
            continue;
        }

        let after_name = tag_start + "<script".len();
        if !is_tag_boundary(lower_source.as_bytes().get(after_name).copied()) {
            search_offset = after_name;
            continue;
        }

        let Some(tag_end) = open_tag_end(&lower_source, after_name) else {
            break;
        };
        let content_start = tag_end + 1;
        // The close tag is the next real `</script...>` (see find_script_close):
        // a legal `</script >` must not be missed, and a `</scriptx>` inside the
        // script text must not be mistaken for it - either error would drop this
        // block and every later block.
        let Some((content_end, close_end)) = find_script_close(&lower_source, content_start) else {
            break;
        };

        blocks.push(ScriptBlock {
            attributes: &source[after_name..tag_end],
            source: &source[content_start..content_end],
            content_start,
        });
        search_offset = close_end;
    }

    blocks
}

/// The index of the `>` that closes an open tag whose attributes start at `from`. A quoted
/// attribute value may contain `>` (Vue's `generic="T extends Record<K, V>"`), so quoted values are
/// skipped; a quote only opens a value right after `=`, as in HTML.
fn open_tag_end(lower_source: &str, from: usize) -> Option<usize> {
    let bytes = lower_source.as_bytes();
    let mut index = from;
    let mut after_equals = false;

    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'>' => return Some(index),
            b'"' | b'\'' if after_equals => {
                index += 1 + lower_source[index + 1..].find(byte as char)?;
                after_equals = false;
            }
            b'=' => after_equals = true,
            byte if byte.is_ascii_whitespace() => {}
            _ => after_equals = false,
        }
        index += 1;
    }

    None
}

/// Finds the next real `</script...>` close tag at or after `from`, skipping
/// pseudo-closes such as `</scriptx>` that appear inside script text: the byte
/// after `</script` must be a tag boundary (whitespace, `/`, `>`, or EOF),
/// mirroring the open-tag check, while still allowing trailing whitespace
/// before `>` (`</script >`). Returns `(content_end, close_end)`.
fn find_script_close(lower_source: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = lower_source.as_bytes();
    let mut scan = from;

    loop {
        let content_end = scan + lower_source[scan..].find("</script")?;
        let after_close_name = content_end + "</script".len();

        if is_tag_boundary(bytes.get(after_close_name).copied()) {
            let close_end = after_close_name + lower_source[after_close_name..].find('>')? + 1;
            return Some((content_end, close_end));
        }

        scan = after_close_name;
    }
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
            // Empty frontmatter (`---` immediately followed by `---`) walks the
            // end back past the content start; clamp so the region is empty
            // rather than an inverted, panicking slice range.
            let content_end = previous_line_end(source, line_start).max(content_start);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_attributes_ignores_substring_matches_before_lang() {
        assert!(matches!(
            language_from_attributes("data-slang=\"x\" lang=\"ts\""),
            ScriptLanguage::Ts
        ));
    }

    #[test]
    fn language_from_attributes_reads_setup_and_lang() {
        assert!(matches!(
            language_from_attributes("setup lang=\"tsx\""),
            ScriptLanguage::Tsx
        ));
    }

    #[test]
    fn language_from_attributes_defaults_to_js() {
        assert!(matches!(language_from_attributes(""), ScriptLanguage::Js));
        assert!(matches!(
            language_from_attributes("setup"),
            ScriptLanguage::Js
        ));
    }

    #[test]
    fn script_blocks_ignore_pseudo_close_tag_glued_to_content() {
        // `</scriptx>` inside script text is not a real close tag; the block must
        // extend to the real `</script>` so later imports are still analyzed.
        let source = "<script>\nconst s = \"</scriptx>\";\nimport foo from './real';\n</script>";
        let blocks = script_blocks(source);

        assert_eq!(blocks.len(), 1);
        assert!(
            blocks[0].source.contains("import foo from './real'"),
            "block source truncated at a pseudo-close tag: {:?}",
            blocks[0].source,
        );
    }

    #[test]
    fn script_blocks_accept_close_tag_with_trailing_whitespace() {
        let source = "<script>\nimport foo from './real';\n</script >";
        let blocks = script_blocks(source);

        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].source.contains("import foo from './real'"));
    }
}
