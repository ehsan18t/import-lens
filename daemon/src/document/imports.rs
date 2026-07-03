use super::{
    positions::{position_at, range_from_offsets},
    script_regions::{ScriptRegion, script_regions_for_document},
    specifier::{get_package_name, is_runtime_package_specifier},
};
use crate::ipc::protocol::{DetectedImport, ImportKind, ImportSyntax};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::Span;
use oxc_syntax::module_record::{
    ExportEntry, ExportImportName, ImportEntry, ImportImportName, ModuleRecord as OxcModuleRecord,
};
use std::collections::HashMap;

pub fn analyze_imports(filename: &str, source: &str) -> Result<Vec<DetectedImport>, String> {
    let mut imports = Vec::new();

    for region in script_regions_for_document(filename, source) {
        imports.extend(imports_from_region(source, &region)?);
    }

    imports.sort_by_key(|item| {
        (
            item.statement_range.start.line,
            item.statement_range.start.character,
        )
    });
    Ok(imports)
}

fn imports_from_region(
    document_source: &str,
    region: &ScriptRegion<'_>,
) -> Result<Vec<DetectedImport>, String> {
    let allocator = Allocator::default();
    let source_type = super::script_regions::source_type_for_region(&region.filename);
    let parsed = Parser::new(&allocator, region.source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse document region {}; errors: {}",
            region.filename,
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut imports = Vec::new();
    imports.extend(imports_from_static_imports(
        document_source,
        region,
        &parsed.module_record,
    ));
    imports.extend(imports_from_static_exports(
        document_source,
        region,
        &parsed.module_record,
    ));
    imports.extend(imports_from_dynamic_imports(
        document_source,
        region,
        &parsed.module_record,
    ));

    Ok(imports)
}

#[derive(Debug)]
struct ImportGroup {
    specifier: String,
    statement_span: Span,
    module_request_span: Span,
    has_default: bool,
    has_namespace: bool,
    named: Vec<String>,
}

fn imports_from_static_imports(
    document_source: &str,
    region: &ScriptRegion<'_>,
    module_record: &OxcModuleRecord<'_>,
) -> Vec<DetectedImport> {
    let mut groups = Vec::<ImportGroup>::new();
    let mut binding_imports = HashMap::<(String, u32, u32), usize>::new();

    for entry in module_record
        .import_entries
        .iter()
        .filter(|entry| !entry.is_type)
    {
        let specifier = entry.module_request.name.as_str();
        if !is_runtime_package_specifier(specifier) {
            continue;
        }

        let key = (
            specifier.to_owned(),
            entry.statement_span.start,
            entry.statement_span.end,
        );
        let index = match binding_imports.get(&key) {
            Some(index) => *index,
            None => {
                groups.push(ImportGroup {
                    specifier: specifier.to_owned(),
                    statement_span: entry.statement_span,
                    module_request_span: entry.module_request.span,
                    has_default: false,
                    has_namespace: false,
                    named: Vec::new(),
                });
                let index = groups.len() - 1;
                binding_imports.insert(key, index);
                index
            }
        };

        apply_import_entry(&mut groups[index], entry);
    }

    for (specifier, requested_modules) in &module_record.requested_modules {
        let specifier = specifier.as_str();
        if !is_runtime_package_specifier(specifier) {
            continue;
        }

        for request in requested_modules
            .iter()
            .filter(|request| request.is_import && !request.is_type)
        {
            let key = (
                specifier.to_owned(),
                request.statement_span.start,
                request.statement_span.end,
            );
            if binding_imports.contains_key(&key) {
                continue;
            }

            groups.push(ImportGroup {
                specifier: specifier.to_owned(),
                statement_span: request.statement_span,
                module_request_span: request.span,
                has_default: false,
                has_namespace: true,
                named: Vec::new(),
            });
            binding_imports.insert(key, groups.len() - 1);
        }
    }

    groups.sort_by_key(|group| (group.statement_span.start, group.module_request_span.start));
    groups
        .into_iter()
        .flat_map(|group| {
            detected_imports_from_group(document_source, region, group, ImportSyntax::Static)
        })
        .collect()
}

fn apply_import_entry(group: &mut ImportGroup, entry: &ImportEntry<'_>) {
    match &entry.import_name {
        ImportImportName::Default(_) => group.has_default = true,
        ImportImportName::NamespaceObject => group.has_namespace = true,
        ImportImportName::Name(name) => group.named.push(name.name.as_str().to_owned()),
    }
}

fn imports_from_static_exports(
    document_source: &str,
    region: &ScriptRegion<'_>,
    module_record: &OxcModuleRecord<'_>,
) -> Vec<DetectedImport> {
    let mut groups = Vec::<ImportGroup>::new();
    let mut indexes = HashMap::<(String, u32, u32), usize>::new();

    for entry in module_record
        .indirect_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
    {
        apply_export_entry(entry, &mut groups, &mut indexes);
    }

    for entry in module_record
        .star_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
    {
        apply_export_entry(entry, &mut groups, &mut indexes);
    }

    groups.sort_by_key(|group| (group.statement_span.start, group.module_request_span.start));
    groups
        .into_iter()
        .flat_map(|group| {
            detected_imports_from_group(document_source, region, group, ImportSyntax::Reexport)
        })
        .collect()
}

fn apply_export_entry(
    entry: &ExportEntry<'_>,
    groups: &mut Vec<ImportGroup>,
    indexes: &mut HashMap<(String, u32, u32), usize>,
) {
    let Some(module_request) = &entry.module_request else {
        return;
    };
    let specifier = module_request.name.as_str();
    if !is_runtime_package_specifier(specifier) {
        return;
    }

    let key = (
        specifier.to_owned(),
        entry.statement_span.start,
        entry.statement_span.end,
    );
    let index = match indexes.get(&key) {
        Some(index) => *index,
        None => {
            groups.push(ImportGroup {
                specifier: specifier.to_owned(),
                statement_span: entry.statement_span,
                module_request_span: module_request.span,
                has_default: false,
                has_namespace: false,
                named: Vec::new(),
            });
            let index = groups.len() - 1;
            indexes.insert(key, index);
            index
        }
    };

    match &entry.import_name {
        ExportImportName::Name(name) => groups[index].named.push(name.name.as_str().to_owned()),
        ExportImportName::All | ExportImportName::AllButDefault => {
            groups[index].has_namespace = true;
        }
        ExportImportName::Null => {}
    }
}

fn imports_from_dynamic_imports(
    document_source: &str,
    region: &ScriptRegion<'_>,
    module_record: &OxcModuleRecord<'_>,
) -> Vec<DetectedImport> {
    let mut imports = module_record
        .dynamic_imports
        .iter()
        .filter_map(|item| {
            let specifier = literal_dynamic_import_specifier(
                &region.source[span_start(item.module_request)..span_end(item.module_request)],
            )?;
            if !is_runtime_package_specifier(&specifier) {
                return None;
            }

            Some(create_detected_import(
                document_source,
                region,
                DetectedImportParts {
                    specifier: &specifier,
                    import_kind: ImportKind::Dynamic,
                    syntax: ImportSyntax::Dynamic,
                    named: Vec::new(),
                    statement_span: item.span,
                    module_request_span: item.module_request,
                },
            ))
        })
        .collect::<Vec<_>>();
    imports.sort_by_key(|item| {
        (
            item.statement_range.start.line,
            item.statement_range.start.character,
        )
    });
    imports
}

fn literal_dynamic_import_specifier(value: &str) -> Option<String> {
    let first = value.chars().next()?;
    let last = value.chars().next_back()?;

    if (first == '\'' || first == '"') && first == last {
        return Some(value[1..value.len() - 1].to_owned());
    }

    if first == '`' && last == '`' && !value.contains("${") {
        return Some(value[1..value.len() - 1].to_owned());
    }

    None
}

fn detected_imports_from_group(
    document_source: &str,
    region: &ScriptRegion<'_>,
    mut group: ImportGroup,
    syntax: ImportSyntax,
) -> Vec<DetectedImport> {
    let mut imports = Vec::new();
    group.named.sort();
    group.named.dedup();

    if group.has_default {
        imports.push(create_detected_import(
            document_source,
            region,
            DetectedImportParts {
                specifier: &group.specifier,
                import_kind: ImportKind::Default,
                syntax,
                named: Vec::new(),
                statement_span: group.statement_span,
                module_request_span: group.module_request_span,
            },
        ));
    }

    if group.has_namespace {
        imports.push(create_detected_import(
            document_source,
            region,
            DetectedImportParts {
                specifier: &group.specifier,
                import_kind: ImportKind::Namespace,
                syntax: if syntax == ImportSyntax::Reexport {
                    ImportSyntax::StarReexport
                } else {
                    syntax
                },
                named: Vec::new(),
                statement_span: group.statement_span,
                module_request_span: group.module_request_span,
            },
        ));
    }

    if !group.named.is_empty() {
        imports.push(create_detected_import(
            document_source,
            region,
            DetectedImportParts {
                specifier: &group.specifier,
                import_kind: ImportKind::Named,
                syntax,
                named: group.named,
                statement_span: group.statement_span,
                module_request_span: group.module_request_span,
            },
        ));
    }

    imports
}

struct DetectedImportParts<'a> {
    specifier: &'a str,
    import_kind: ImportKind,
    syntax: ImportSyntax,
    named: Vec<String>,
    statement_span: Span,
    module_request_span: Span,
}

fn create_detected_import(
    document_source: &str,
    region: &ScriptRegion<'_>,
    parts: DetectedImportParts<'_>,
) -> DetectedImport {
    let statement_start = region.offset + span_start(parts.statement_span);
    let statement_end = region.offset + span_end(parts.statement_span);
    let specifier_start = region.offset + span_start(parts.module_request_span);
    let quote_end = region.offset + span_end(parts.module_request_span);
    let line = position_at(document_source, statement_start).line;

    DetectedImport {
        specifier: parts.specifier.to_owned(),
        package_name: get_package_name(parts.specifier),
        named: parts.named,
        import_kind: parts.import_kind,
        syntax: parts.syntax,
        runtime: region.runtime,
        line,
        quote_end: position_at(document_source, quote_end),
        specifier_range: range_from_offsets(document_source, specifier_start, quote_end),
        statement_range: range_from_offsets(document_source, statement_start, statement_end),
    }
}

fn span_start(span: Span) -> usize {
    span.start as usize
}

fn span_end(span: Span) -> usize {
    span.end as usize
}
