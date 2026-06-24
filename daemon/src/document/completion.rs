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
    source: &str,
    offset: usize,
) -> Option<NamedImportCompletionContext> {
    let allocator = Allocator::default();
    let parsed = Parser::new(
        &allocator,
        source,
        SourceType::from_path(Path::new("import-lens-completion.tsx"))
            .unwrap_or_else(|_| SourceType::tsx()),
    )
    .parse();

    if parsed.panicked || !parsed.errors.is_empty() {
        return None;
    }

    completion_context_from_module_record(source, offset, &parsed.module_record)
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
        let range = named_import_member_range(source, group.statement_span)?;
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
