use super::script_regions::{ScriptRegion, script_regions_for_document};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use oxc_syntax::module_record::{ImportImportName, ModuleRecord as OxcModuleRecord};
use std::{collections::HashMap, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedImportCompletionContext {
    pub specifier: String,
    pub imported_names: Vec<String>,
}

pub fn named_import_completion_context(
    filename: &str,
    source: &str,
    utf16_cursor_offset: usize,
) -> Option<NamedImportCompletionContext> {
    let offset = byte_offset_for_utf16(source, utf16_cursor_offset);

    for region in script_regions_for_document(filename, source) {
        let region_end = region.offset + region.source.len();
        if offset < region.offset || offset > region_end {
            continue;
        }

        if let Some(context) = region_completion_context(&region, offset - region.offset) {
            return Some(context);
        }
    }

    None
}

// VS Code's `document.offsetAt` counts UTF-16 code units, while oxc spans are
// byte offsets; the two only coincide for pure-ASCII prefixes.
fn byte_offset_for_utf16(source: &str, utf16_offset: usize) -> usize {
    let mut utf16_seen = 0;

    for (byte_index, char) in source.char_indices() {
        if utf16_seen >= utf16_offset {
            return byte_index;
        }
        utf16_seen += char.len_utf16();
    }

    source.len()
}

fn region_completion_context(
    region: &ScriptRegion<'_>,
    offset: usize,
) -> Option<NamedImportCompletionContext> {
    let allocator = Allocator::default();
    let source_type =
        SourceType::from_path(Path::new(&region.filename)).unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, region.source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return None;
    }

    completion_context_from_module_record(region.source, offset, &parsed.module_record)
}

fn completion_context_from_module_record(
    source: &str,
    offset: usize,
    module_record: &OxcModuleRecord<'_>,
) -> Option<NamedImportCompletionContext> {
    let mut groups = HashMap::<(u32, u32), NamedImportGroup>::new();

    for entry in module_record
        .import_entries
        .iter()
        .filter(|entry| !entry.is_type)
    {
        let key = (entry.statement_span.start, entry.statement_span.end);
        let group = groups.entry(key).or_insert_with(|| NamedImportGroup {
            specifier: entry.module_request.name.as_str().to_owned(),
            statement_span: entry.statement_span,
            imported_names: Vec::new(),
        });

        if let ImportImportName::Name(name) = &entry.import_name {
            group.imported_names.push(name.name.as_str().to_owned());
        }
    }

    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by_key(|group| group.statement_span.start);

    for mut group in groups {
        let Some(range) = named_import_member_range(source, group.statement_span) else {
            continue;
        };
        if offset < range.start || offset > range.end {
            continue;
        }

        group.imported_names.sort();
        group.imported_names.dedup();
        return Some(NamedImportCompletionContext {
            specifier: group.specifier,
            imported_names: group.imported_names,
        });
    }

    None
}

#[derive(Debug)]
struct NamedImportGroup {
    specifier: String,
    statement_span: Span,
    imported_names: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct OffsetRange {
    start: usize,
    end: usize,
}

#[cfg(test)]
mod tests {
    use super::named_import_completion_context;

    #[test]
    fn completion_skips_earlier_imports_without_braces() {
        let source = "import React from 'react';\nimport { map } from 'lodash';\n";
        let cursor = source.rfind('{').expect("brace should exist") + 1;

        let context = named_import_completion_context("main.tsx", source, cursor)
            .expect("completion context");

        assert_eq!(context.specifier, "lodash");
        assert_eq!(context.imported_names, vec!["map"]);
    }

    #[test]
    fn completion_works_inside_vue_script_blocks() {
        let source = "<template><div /></template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue';\n</script>\n";
        let cursor = source.rfind('{').expect("brace should exist") + 1;

        let context = named_import_completion_context("component.vue", source, cursor)
            .expect("completion context inside script block");

        assert_eq!(context.specifier, "vue");
        assert_eq!(context.imported_names, vec!["ref"]);
    }

    #[test]
    fn completion_parses_ts_documents_as_typescript_not_tsx() {
        let source = "import { map } from 'lodash';\nconst value = <string>JSON.parse('\"x\"');\n";
        let cursor = source.find('{').expect("brace should exist") + 1;

        let context = named_import_completion_context("main.ts", source, cursor)
            .expect("angle-bracket assertion should not break completion");

        assert_eq!(context.specifier, "lodash");
    }

    #[test]
    fn completion_accepts_utf16_cursor_offsets() {
        let source = "const s = '\u{20AC}\u{20AC}';\nimport { map } from 'lodash';\n";
        let byte_cursor = source.rfind('{').expect("brace should exist") + 1;
        let utf16_cursor: usize = source[..byte_cursor].chars().map(char::len_utf16).sum();
        assert_ne!(byte_cursor, utf16_cursor);

        let context = named_import_completion_context("main.ts", source, utf16_cursor)
            .expect("UTF-16 offset should resolve");

        assert_eq!(context.specifier, "lodash");
    }
}

fn named_import_member_range(source: &str, statement_span: Span) -> Option<OffsetRange> {
    let statement_start = statement_span.start as usize;
    let statement_end = statement_span.end as usize;
    let statement = source.get(statement_start..statement_end)?;
    let open_brace = statement.find('{')?;
    let close_brace = statement[open_brace + 1..].find('}')? + open_brace + 1;

    Some(OffsetRange {
        start: statement_start + open_brace + 1,
        end: statement_start + close_brace,
    })
}
