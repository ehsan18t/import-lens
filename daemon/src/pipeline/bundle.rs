use crate::pipeline::{
    graph::{ModuleGraph, ModuleId, ModuleRecord},
    reachability::ReachableExports,
};
use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};
use std::collections::{HashMap, HashSet};

pub fn bundle_reachable_modules(
    graph: &ModuleGraph,
    reachable: &ReachableExports,
) -> Result<String, String> {
    let included = included_module_ids(graph, reachable);
    let mut source = String::new();
    let mut deduplicated_external_imports = HashMap::new();

    for module in graph
        .modules
        .iter()
        .filter(|module| included.contains_key(&module.id))
    {
        for ext in &module.external_imports {
            deduplicated_external_imports
                .entry(ext.specifier.clone())
                .or_insert_with(Vec::new)
                .push(ext.clone());
        }

        let keep_all_exports = included.get(&module.id).copied().unwrap_or(false);
        let rewritten = rewrite_module(graph, module, reachable, keep_all_exports)?;
        if !rewritten.trim().is_empty() {
            source.push_str(&rewritten);
            source.push('\n');
        }
    }

    let mut synthetic_imports = String::new();
    let mut specifiers: Vec<_> = deduplicated_external_imports.keys().collect();
    specifiers.sort();

    for specifier in specifiers {
        let edges = deduplicated_external_imports.get(specifier).unwrap();
        let mut default_local = None;
        let mut namespace_local = None;
        let mut named_imports = Vec::new();

        for edge in edges {
            if edge.imported_name.is_empty() {
                continue;
            } else if edge.imported_name == "default" {
                default_local = Some(edge.local_name.clone());
            } else if edge.imported_name == "*" {
                namespace_local = Some(edge.local_name.clone());
            } else {
                if edge.imported_name == edge.local_name {
                    named_imports.push(edge.imported_name.clone());
                } else {
                    named_imports.push(format!("{} as {}", edge.imported_name, edge.local_name));
                }
            }
        }

        let has_bindings =
            default_local.is_some() || namespace_local.is_some() || !named_imports.is_empty();

        if let Some(local) = default_local {
            synthetic_imports.push_str(&format!("import {} from '{}';\n", local, specifier));
        }
        if let Some(local) = namespace_local {
            synthetic_imports.push_str(&format!("import * as {} from '{}';\n", local, specifier));
        }
        if !named_imports.is_empty() {
            named_imports.sort();
            named_imports.dedup();
            synthetic_imports.push_str(&format!(
                "import {{ {} }} from '{}';\n",
                named_imports.join(", "),
                specifier
            ));
        }

        if !has_bindings {
            synthetic_imports.push_str(&format!("import '{}';\n", specifier));
        }
    }

    synthetic_imports.push_str(&source);
    Ok(synthetic_imports)
}

pub fn included_module_ids(
    graph: &ModuleGraph,
    reachable: &ReachableExports,
) -> HashMap<ModuleId, bool> {
    let mut included = HashMap::new();

    for module in &graph.modules {
        if reachable.contains_module(&module.path) {
            let keep_all_exports = reachable.is_full_module(&module.path);
            include_module_with_imports(graph, module.id, keep_all_exports, &mut included);
        }
    }

    included
}

fn include_module_with_imports(
    graph: &ModuleGraph,
    module_id: ModuleId,
    keep_all_exports: bool,
    included: &mut HashMap<ModuleId, bool>,
) {
    let was_included_as_full = included.insert(
        module_id,
        included.get(&module_id).copied().unwrap_or(false) || keep_all_exports,
    );
    if was_included_as_full == Some(true) && keep_all_exports {
        return;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return;
    };

    for import in &module.imports {
        if let Some(target_id) = graph.module_id_by_path(&import.resolved_path) {
            include_module_with_imports(graph, target_id, true, included);
        }
    }

    if keep_all_exports {
        for reexport in &module.reexports {
            if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path) {
                include_module_with_imports(graph, target_id, true, included);
            }
        }
        for star_export in &module.star_exports {
            if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path) {
                include_module_with_imports(graph, target_id, true, included);
            }
        }
    }
}

fn rewrite_module(
    graph: &ModuleGraph,
    module: &ModuleRecord,
    reachable: &ReachableExports,
    keep_all_exports: bool,
) -> Result<String, String> {
    let mut replacements = Vec::new();

    for import_span in &module.import_statement_spans {
        replacements.push(Replacement::remove(import_span.0, import_span.1));
    }
    for export_span in &module.export_specifier_statement_spans {
        replacements.push(Replacement::remove(export_span.0, export_span.1));
    }
    for reexport in &module.reexports {
        replacements.push(Replacement::remove(
            reexport.statement_start,
            reexport.statement_end,
        ));
    }
    for star_export in &module.star_exports {
        replacements.push(Replacement::remove(
            star_export.statement_start,
            star_export.statement_end,
        ));
    }

    let mut seen_export_spans = HashSet::new();
    for export in &module.exports {
        if !seen_export_spans.insert((export.statement_start, export.statement_end)) {
            continue;
        }

        let span_exports = module
            .exports
            .iter()
            .filter(|candidate| {
                candidate.statement_start == export.statement_start
                    && candidate.statement_end == export.statement_end
            })
            .collect::<Vec<_>>();
        let keep_statement = keep_all_exports
            || span_exports.iter().any(|export| {
                reachable.contains_module_symbol(&module.path, &export.exported_name)
            });

        if keep_statement {
            replacements.extend(transform_export_statement(
                module,
                export.statement_start,
                export.statement_end,
            )?);
        } else {
            replacements.push(Replacement::remove(
                export.statement_start,
                export.statement_end,
            ));
        }
    }

    let renames = rename_map(graph, module)?;
    let rename_replacements = semantic_rename_replacements(module, &renames, &replacements)?;
    replacements.extend(rename_replacements);

    let mut rewritten = apply_replacements(&module.source, replacements)?;
    let anchors = usage_anchors(module, reachable, keep_all_exports);
    if !anchors.is_empty() {
        rewritten.push_str("\n;");
        rewritten.push_str(&anchors);
    }

    Ok(rewritten)
}

fn transform_export_statement(
    module: &ModuleRecord,
    start: usize,
    end: usize,
) -> Result<Vec<Replacement>, String> {
    let statement = module.source.get(start..end).ok_or_else(|| {
        format!(
            "invalid export span {start}..{end} in {}",
            module.path.display()
        )
    })?;
    let trimmed = statement.trim_start();
    let leading_len = statement.len() - trimmed.len();
    let export_start = start + leading_len;

    if trimmed.starts_with("export {") || trimmed.starts_with("export type {") {
        return Ok(vec![Replacement::remove(start, end)]);
    }

    if let Some(after_default) = trimmed.strip_prefix("export default") {
        let default_end = export_start + "export default".len();
        let trimmed_after_default = after_default.trim_start();
        let whitespace_len = after_default.len() - trimmed_after_default.len();
        if is_named_default_declaration(trimmed_after_default) {
            return Ok(vec![Replacement::remove(
                export_start,
                default_end + whitespace_len,
            )]);
        }

        let default_name = module_binding_name(module.id, "default");
        return Ok(vec![Replacement::replace(
            export_start,
            default_end,
            format!("const {default_name} ="),
        )]);
    }

    if trimmed.starts_with("export ") {
        return Ok(vec![Replacement::remove(
            export_start,
            export_start + "export ".len(),
        )]);
    }

    Ok(Vec::new())
}

fn is_named_default_declaration(trimmed_after_default: &str) -> bool {
    if let Some(after_function) = trimmed_after_default.strip_prefix("function") {
        return !after_function.trim_start().starts_with('(');
    }

    if let Some(after_class) = trimmed_after_default.strip_prefix("class") {
        return !after_class.trim_start().starts_with('{');
    }

    false
}

fn rename_map(
    graph: &ModuleGraph,
    module: &ModuleRecord,
) -> Result<HashMap<String, String>, String> {
    let mut renames = HashMap::new();

    for binding in &module.local_bindings {
        renames.insert(binding.clone(), module_binding_name(module.id, binding));
    }

    for import in &module.imports {
        let Some(target_id) = graph.module_id_by_path(&import.resolved_path) else {
            continue;
        };
        for binding in &import.imported_bindings {
            if binding.imported_name == "*" {
                renames.insert(
                    binding.local_name.clone(),
                    module_binding_name(target_id, "namespace"),
                );
                continue;
            }

            let target_name = resolve_export_binding(graph, target_id, &binding.imported_name)
                .unwrap_or_else(|| module_binding_name(target_id, &binding.imported_name));
            renames.insert(binding.local_name.clone(), target_name);
        }
    }

    Ok(renames)
}

fn resolve_export_binding(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
) -> Option<String> {
    let module = graph.module_by_id(module_id)?;
    if let Some(export) = module
        .exports
        .iter()
        .find(|export| export.exported_name == exported_name)
    {
        return Some(module_binding_name(module_id, &export.local_name));
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path)
            && let Some(binding) = resolve_export_binding(graph, target_id, &reexport.imported_name)
        {
            return Some(binding);
        }
    }

    for star_export in &module.star_exports {
        let target_id = graph.module_id_by_path(&star_export.resolved_path)?;
        if let Some(binding) = resolve_export_binding(graph, target_id, exported_name) {
            return Some(binding);
        }
    }

    None
}

fn semantic_rename_replacements(
    module: &ModuleRecord,
    renames: &HashMap<String, String>,
    protected_replacements: &[Replacement],
) -> Result<Vec<Replacement>, String> {
    if renames.is_empty() {
        return Ok(Vec::new());
    }

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(&module.path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, &module.source, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "failed to parse module for renaming {}; errors: {}",
            module.path.display(),
            parsed
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&parsed.program);
    if !semantic.errors.is_empty() {
        return Err(format!(
            "semantic validation failed for renaming {}; errors: {}",
            module.path.display(),
            semantic
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let semantic = semantic.semantic;
    let scoping = semantic.scoping();
    let mut replacements = Vec::new();
    let mut seen_spans = HashSet::new();

    for symbol_id in scoping.iter_bindings_in(scoping.root_scope_id()) {
        let symbol_name = scoping.symbol_name(symbol_id);
        let Some(new_name) = renames.get(symbol_name) else {
            continue;
        };

        push_semantic_rename(
            &module.source,
            scoping.symbol_span(symbol_id),
            new_name,
            protected_replacements,
            &mut seen_spans,
            &mut replacements,
        )?;

        for reference in semantic.symbol_references(symbol_id) {
            push_semantic_rename(
                &module.source,
                semantic.reference_span(reference),
                new_name,
                protected_replacements,
                &mut seen_spans,
                &mut replacements,
            )?;
        }
    }

    Ok(replacements)
}

fn push_semantic_rename(
    source: &str,
    span: Span,
    new_name: &str,
    protected_replacements: &[Replacement],
    seen_spans: &mut HashSet<(usize, usize)>,
    replacements: &mut Vec<Replacement>,
) -> Result<(), String> {
    let start = span.start as usize;
    let end = span.end as usize;
    let Some(original_name) = source.get(start..end) else {
        return Err(format!("invalid semantic rename span {start}..{end}"));
    };
    if !seen_spans.insert((start, end))
        || span_overlaps_replacements(start, end, protected_replacements)
    {
        return Ok(());
    }

    let value = if is_object_shorthand_occurrence(source, start, end) {
        format!("{original_name}: {new_name}")
    } else {
        new_name.to_owned()
    };
    replacements.push(Replacement::replace(start, end, value));
    Ok(())
}

fn span_overlaps_replacements(start: usize, end: usize, replacements: &[Replacement]) -> bool {
    replacements
        .iter()
        .any(|replacement| start < replacement.end && end > replacement.start)
}

#[derive(Debug)]
struct Replacement {
    start: usize,
    end: usize,
    value: String,
}

impl Replacement {
    fn remove(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            value: String::new(),
        }
    }

    fn replace(start: usize, end: usize, value: String) -> Self {
        Self { start, end, value }
    }
}

fn apply_replacements(source: &str, mut replacements: Vec<Replacement>) -> Result<String, String> {
    replacements.sort_by(|a, b| {
        b.start
            .cmp(&a.start)
            .then_with(|| b.end.cmp(&a.end))
            .then_with(|| a.value.len().cmp(&b.value.len()))
    });

    let source_len = source.len();
    let mut valid_replacements = Vec::new();
    let mut last_start = source_len;

    for replacement in replacements {
        if replacement.start > replacement.end || replacement.end > source_len {
            return Err(format!(
                "invalid replacement span {}..{}",
                replacement.start, replacement.end
            ));
        }
        if replacement.end > last_start {
            continue;
        }
        last_start = replacement.start;
        valid_replacements.push(replacement);
    }

    valid_replacements.reverse();

    let mut output = String::with_capacity(source.len());
    let mut last_end = 0;

    for replacement in valid_replacements {
        output.push_str(&source[last_end..replacement.start]);
        output.push_str(&replacement.value);
        last_end = replacement.end;
    }
    output.push_str(&source[last_end..]);

    Ok(output)
}

fn usage_anchors(
    module: &ModuleRecord,
    reachable: &ReachableExports,
    keep_all_exports: bool,
) -> String {
    let mut anchors = String::new();
    for export in &module.exports {
        if keep_all_exports || reachable.contains_module_symbol(&module.path, &export.exported_name)
        {
            anchors.push_str("__importLensUse(");
            anchors.push_str(&module_binding_name(module.id, &export.local_name));
            anchors.push_str(");\n");
        }
    }
    anchors
}

fn is_object_shorthand_occurrence(source: &str, start: usize, end: usize) -> bool {
    enclosing_delimiter(source, start) == Some(b'{')
        && matches!(previous_significant_byte(source, start), Some(b'{' | b','))
        && matches!(next_significant_byte(source, end), Some(b'}' | b',' | b'='))
}

fn previous_significant_byte(source: &str, start: usize) -> Option<u8> {
    source.as_bytes()[..start]
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn next_significant_byte(source: &str, end: usize) -> Option<u8> {
    source.as_bytes()[end..]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn enclosing_delimiter(source: &str, position: usize) -> Option<u8> {
    let bytes = source.as_bytes();
    let mut stack = Vec::new();
    let mut index = 0;

    while index < position && index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\'' | b'"' | b'`' => {
                index = skip_quoted(source, index, byte);
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = skip_line_comment(source, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(source, index);
            }
            b'{' | b'[' | b'(' => {
                stack.push(byte);
                index += 1;
            }
            b'}' => {
                pop_matching_delimiter(&mut stack, b'{');
                index += 1;
            }
            b']' => {
                pop_matching_delimiter(&mut stack, b'[');
                index += 1;
            }
            b')' => {
                pop_matching_delimiter(&mut stack, b'(');
                index += 1;
            }
            _ => {
                index = next_char_end(source, index);
            }
        }
    }

    stack.last().copied()
}

fn pop_matching_delimiter(stack: &mut Vec<u8>, expected: u8) {
    if stack.last() == Some(&expected) {
        stack.pop();
    }
}

fn skip_quoted(source: &str, start: usize, quote: u8) -> usize {
    let bytes = source.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        let byte = bytes[index];
        index = next_char_end(source, index);
        if byte == b'\\' && index < bytes.len() {
            index = next_char_end(source, index);
        } else if byte == quote {
            break;
        }
    }
    index
}

fn skip_line_comment(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte == b'\n' {
            break;
        }
    }
    index
}

fn skip_block_comment(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte == b'*' && bytes.get(index) == Some(&b'/') {
            index += 1;
            break;
        }
    }
    index
}

fn next_char_end(source: &str, start: usize) -> usize {
    source[start..]
        .chars()
        .next()
        .map(|character| start + character.len_utf8())
        .unwrap_or(start + 1)
}

fn module_binding_name(module_id: ModuleId, name: &str) -> String {
    format!("__il_m{}_{}", module_id.0, sanitize_identifier(name))
}

fn sanitize_identifier(name: &str) -> String {
    let mut sanitized = String::new();
    for (index, byte) in name.bytes().enumerate() {
        let valid = if index == 0 {
            is_identifier_start(byte)
        } else {
            is_identifier_continue(byte)
        };
        sanitized.push(if valid { byte as char } else { '_' });
    }
    if sanitized.is_empty() {
        return "_".to_owned();
    }
    sanitized
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphabetic()
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}
