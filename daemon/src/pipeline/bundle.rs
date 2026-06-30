use crate::{
    ipc::protocol::ModuleContribution,
    pipeline::{
        graph::{ModuleGraph, ModuleId, ModuleRecord},
        reachability::ReachableExports,
        replacements::{Replacement, apply_replacements, span_overlaps_replacements},
    },
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentTargetPropertyIdentifier, BindingPattern, BindingProperty, Expression,
    ObjectProperty, Program,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct BundledModules {
    pub source: String,
    pub minifier_source: String,
    pub contributions: Vec<ModuleContribution>,
}

pub fn bundle_reachable_modules(
    graph: &ModuleGraph,
    reachable: &ReachableExports,
) -> Result<String, String> {
    bundle_reachable_modules_with_metadata(graph, reachable).map(|bundled| bundled.source)
}

pub fn bundle_reachable_modules_with_metadata(
    graph: &ModuleGraph,
    reachable: &ReachableExports,
) -> Result<BundledModules, String> {
    let mut expanded_reachable = reachable.clone();
    let included = included_module_ids_with_reachable(graph, &mut expanded_reachable);
    let mut source = String::new();
    let mut minifier_source = String::new();
    let mut contributions = Vec::new();
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
        let rewritten = rewrite_module(graph, module, &expanded_reachable, keep_all_exports)?;
        if !rewritten.trim().is_empty() {
            contributions.push(ModuleContribution {
                path: module.path.to_string_lossy().to_string(),
                bytes: rewritten.len() as u64,
            });
            source.push_str(&rewritten);
            source.push('\n');

            minifier_source.push_str(&rewritten);
            let markers = usage_markers(module, &expanded_reachable, keep_all_exports);
            if !markers.is_empty() {
                minifier_source.push('\n');
                minifier_source.push_str(&markers);
            }
            minifier_source.push('\n');
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

    let mut bundled_source = synthetic_imports.clone();
    bundled_source.push_str(&source);
    let mut bundled_minifier_source = synthetic_imports;
    bundled_minifier_source.push_str(&minifier_source);

    Ok(BundledModules {
        source: bundled_source,
        minifier_source: bundled_minifier_source,
        contributions,
    })
}

fn included_module_ids_with_reachable(
    graph: &ModuleGraph,
    reachable: &mut ReachableExports,
) -> HashMap<ModuleId, bool> {
    let mut included = HashMap::new();

    for module in &graph.modules {
        if reachable.contains_module(&module.path) {
            let keep_all_exports = reachable.is_full_module(&module.path);
            include_module_with_imports(
                graph,
                module.id,
                keep_all_exports,
                reachable,
                &mut included,
            );
        }
    }

    included
}

fn include_module_with_imports(
    graph: &ModuleGraph,
    module_id: ModuleId,
    keep_all_exports: bool,
    reachable: &mut ReachableExports,
    included: &mut HashMap<ModuleId, bool>,
) {
    let previous_keep_all = included.get(&module_id).copied();
    let next_keep_all = previous_keep_all.unwrap_or(false) || keep_all_exports;
    if previous_keep_all == Some(next_keep_all) {
        return;
    }
    included.insert(module_id, next_keep_all);

    let Some(module) = graph.module_by_id(module_id) else {
        return;
    };
    if next_keep_all {
        reachable.mark_full_module(module.path.clone());
    } else {
        reachable.mark_module(module.path.clone());
    }

    for import in &module.imports {
        if let Some(target_id) = graph.module_id_by_path(&import.resolved_path) {
            let target_keep_all =
                keep_all_exports || import.imported_names.iter().any(|name| name == "*");
            if let Some(target) = graph.module_by_id(target_id) {
                if target_keep_all {
                    reachable.mark_full_module(target.path.clone());
                } else if import.imported_names.is_empty() {
                    reachable.mark_module(target.path.clone());
                } else {
                    for imported_name in &import.imported_names {
                        reachable.mark_module_symbol(target.path.clone(), imported_name.clone());
                    }
                }
            }
            include_module_with_imports(graph, target_id, target_keep_all, reachable, included);
        }
    }

    if next_keep_all {
        for reexport in &module.reexports {
            if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path) {
                include_module_with_imports(graph, target_id, true, reachable, included);
            }
        }
        for star_export in &module.star_exports {
            if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path) {
                include_module_with_imports(graph, target_id, true, reachable, included);
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

    let rewritten = apply_replacements(&module.source, replacements)?;
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
    let source_type = SourceType::mjs();
    let parsed = Parser::new(&allocator, &module.source, source_type).parse();
    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse module for renaming {}; errors: {}",
            module.path.display(),
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let semantic = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "semantic validation failed for renaming {}; errors: {}",
            module.path.display(),
            semantic
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let shorthand_spans = shorthand_identifier_spans(&parsed.program);
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
            &shorthand_spans,
            protected_replacements,
            &mut seen_spans,
            &mut replacements,
        )?;

        for reference in semantic.symbol_references(symbol_id) {
            push_semantic_rename(
                &module.source,
                semantic.reference_span(reference),
                new_name,
                &shorthand_spans,
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
    shorthand_spans: &HashSet<(usize, usize)>,
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

    let value = if shorthand_spans.contains(&(start, end)) {
        format!("{original_name}: {new_name}")
    } else {
        new_name.to_owned()
    };
    replacements.push(Replacement::replace(start, end, value));
    Ok(())
}

fn usage_markers(
    module: &ModuleRecord,
    reachable: &ReachableExports,
    keep_all_exports: bool,
) -> String {
    let mut markers = String::new();
    for export in &module.exports {
        if keep_all_exports || reachable.contains_module_symbol(&module.path, &export.exported_name)
        {
            markers.push_str("export { ");
            markers.push_str(&module_binding_name(module.id, &export.local_name));
            markers.push_str(" as __importLensUse_");
            markers.push_str(&module_binding_name(module.id, &export.exported_name));
            markers.push_str(" };\n");
        }
    }
    markers
}

fn shorthand_identifier_spans(program: &Program<'_>) -> HashSet<(usize, usize)> {
    let mut collector = ShorthandIdentifierCollector::default();
    collector.visit_program(program);
    collector.spans
}

#[derive(Default)]
struct ShorthandIdentifierCollector {
    spans: HashSet<(usize, usize)>,
}

impl<'a> Visit<'a> for ShorthandIdentifierCollector {
    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if property.shorthand
            && let Expression::Identifier(identifier) = &property.value
        {
            self.spans.insert(span_bounds(identifier.span));
        }

        walk::walk_object_property(self, property);
    }

    fn visit_binding_property(&mut self, property: &BindingProperty<'a>) {
        if property.shorthand {
            collect_binding_pattern_spans(&property.value, &mut self.spans);
        }

        walk::walk_binding_property(self, property);
    }

    fn visit_assignment_target_property_identifier(
        &mut self,
        property: &AssignmentTargetPropertyIdentifier<'a>,
    ) {
        self.spans.insert(span_bounds(property.binding.span));
        walk::walk_assignment_target_property_identifier(self, property);
    }
}

fn collect_binding_pattern_spans(
    pattern: &BindingPattern<'_>,
    spans: &mut HashSet<(usize, usize)>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            spans.insert(span_bounds(identifier.span));
        }
        BindingPattern::AssignmentPattern(pattern) => {
            collect_binding_pattern_spans(&pattern.left, spans);
        }
        BindingPattern::ObjectPattern(pattern) => {
            for property in &pattern.properties {
                collect_binding_pattern_spans(&property.value, spans);
            }
            if let Some(rest) = &pattern.rest {
                collect_binding_pattern_spans(&rest.argument, spans);
            }
        }
        BindingPattern::ArrayPattern(pattern) => {
            for element in pattern.elements.iter().flatten() {
                collect_binding_pattern_spans(element, spans);
            }
            if let Some(rest) = &pattern.rest {
                collect_binding_pattern_spans(&rest.argument, spans);
            }
        }
        _ => {}
    }
}

fn span_bounds(span: Span) -> (usize, usize) {
    (span.start as usize, span.end as usize)
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
