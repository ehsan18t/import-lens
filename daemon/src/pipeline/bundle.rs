use crate::{
    ipc::protocol::ModuleContribution,
    pipeline::{
        graph::{ExternalImportEdge, ModuleGraph, ModuleId, ModuleRecord},
        reachability::ReachableExports,
        replacements::{Replacement, apply_replacements, span_overlaps_replacements},
    },
};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
};

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
    let mut deduplicated_external_imports = HashMap::<&str, Vec<&ExternalImportEdge>>::new();

    let included_modules = graph
        .modules
        .iter()
        .filter(|module| included.contains_key(&module.id))
        .collect::<Vec<_>>();
    let mut external_specifiers = included_modules
        .iter()
        .flat_map(|module| module.external_imports.iter())
        .map(|ext| ext.specifier.clone())
        .collect::<Vec<_>>();
    external_specifiers.sort_unstable();
    external_specifiers.dedup();
    let external_indexes = external_specifiers
        .iter()
        .enumerate()
        .map(|(index, specifier)| (specifier.clone(), index))
        .collect::<HashMap<String, usize>>();

    for module in included_modules {
        for ext in &module.external_imports {
            deduplicated_external_imports
                .entry(ext.specifier.as_str())
                .or_default()
                .push(ext);
        }

        let keep_all_exports = included.get(&module.id).copied().unwrap_or(false);
        let renames = rename_map(graph, module, &external_indexes)?;
        let rewritten = rewrite_module(module, &expanded_reachable, keep_all_exports, &renames)?;
        if !rewritten.trim().is_empty() {
            contributions.push(ModuleContribution {
                path: module.path.to_string_lossy().to_string(),
                bytes: rewritten.len() as u64,
            });
            source.push_str(&rewritten);
            source.push('\n');

            minifier_source.push_str(&rewritten);
            let markers = usage_markers(module, &expanded_reachable, keep_all_exports, &renames);
            if !markers.is_empty() {
                minifier_source.push('\n');
                minifier_source.push_str(&markers);
            }
            minifier_source.push('\n');
        }
    }

    let mut synthetic_imports = String::new();
    let mut specifiers = deduplicated_external_imports
        .keys()
        .copied()
        .collect::<Vec<_>>();
    specifiers.sort_unstable();

    for specifier in specifiers {
        let index = external_indexes[specifier];
        let edges = deduplicated_external_imports.get(specifier).unwrap();
        let mut has_default = false;
        let mut has_namespace = false;
        let mut named_imports = Vec::new();
        let mut has_bindings = false;

        for edge in edges {
            match edge.imported_name.as_str() {
                "" => {}
                "default" => has_default = true,
                "*" => has_namespace = true,
                name => named_imports.push(name.to_owned()),
            }
            has_bindings |= !edge.imported_name.is_empty();
        }

        if has_default {
            writeln!(
                synthetic_imports,
                "import {} from '{specifier}';",
                external_binding_name(index, "default")
            )
            .expect("writing to String should not fail");
        }
        if has_namespace {
            writeln!(
                synthetic_imports,
                "import * as {} from '{specifier}';",
                external_binding_name(index, "*")
            )
            .expect("writing to String should not fail");
        }
        if !named_imports.is_empty() {
            named_imports.sort_unstable();
            named_imports.dedup();
            let named = named_imports
                .iter()
                .map(|name| format!("{name} as {}", external_binding_name(index, name)))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                synthetic_imports,
                "import {{ {named} }} from '{specifier}';"
            )
            .expect("writing to String should not fail");
        }

        if !has_bindings {
            writeln!(synthetic_imports, "import '{specifier}';")
                .expect("writing to String should not fail");
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
    let mut processed_bindings = HashMap::<ModuleId, HashSet<String>>::new();

    for module in &graph.modules {
        if reachable.contains_module(&module.path) {
            let keep_all_exports = reachable.is_full_module(&module.path);
            include_module_with_imports(
                graph,
                module.id,
                keep_all_exports,
                reachable,
                &mut included,
                &mut processed_bindings,
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
    processed_bindings: &mut HashMap<ModuleId, HashSet<String>>,
) {
    let previous_keep_all = included.get(&module_id).copied();
    let next_keep_all = previous_keep_all.unwrap_or(false) || keep_all_exports;

    let Some(module) = graph.module_by_id(module_id) else {
        return;
    };

    let retained_bindings = retained_binding_names(module, reachable);
    if previous_keep_all == Some(next_keep_all)
        && !retained_bindings_changed(
            module_id,
            next_keep_all,
            &retained_bindings,
            processed_bindings,
        )
    {
        return;
    }
    included.insert(module_id, next_keep_all);

    if next_keep_all {
        reachable.mark_full_module(module.path.clone());
    } else {
        reachable.mark_module(module.path.clone());
    }

    let include_all_static_imports =
        next_keep_all || !module_has_reachable_export(module, reachable);
    for import in &module.imports {
        if let Some(target_id) = graph.module_id_by_path(&import.resolved_path) {
            let retained_import_names =
                retained_import_names(import, include_all_static_imports, &retained_bindings);
            if retained_import_names.is_empty() && !import.imported_names.is_empty() {
                continue;
            }
            let target_keep_all =
                include_all_static_imports || retained_import_names.iter().any(|name| name == "*");
            mark_reachable_import_target(
                graph,
                target_id,
                &retained_import_names,
                target_keep_all,
                reachable,
            );
            include_module_with_imports(
                graph,
                target_id,
                target_keep_all,
                reachable,
                included,
                processed_bindings,
            );
        }
    }

    if next_keep_all {
        for reexport in &module.reexports {
            if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path) {
                include_module_with_imports(
                    graph,
                    target_id,
                    true,
                    reachable,
                    included,
                    processed_bindings,
                );
            }
        }
        for star_export in &module.star_exports {
            if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path) {
                include_module_with_imports(
                    graph,
                    target_id,
                    true,
                    reachable,
                    included,
                    processed_bindings,
                );
            }
        }
    }
}

fn retained_bindings_changed(
    module_id: ModuleId,
    keep_all_exports: bool,
    retained_bindings: &HashSet<String>,
    processed_bindings: &mut HashMap<ModuleId, HashSet<String>>,
) -> bool {
    if keep_all_exports {
        return false;
    }

    let processed = processed_bindings.entry(module_id).or_default();
    let changed = retained_bindings
        .iter()
        .any(|binding| !processed.contains(binding));
    processed.extend(retained_bindings.iter().cloned());
    changed
}

fn retained_binding_names(module: &ModuleRecord, reachable: &ReachableExports) -> HashSet<String> {
    let mut retained = module
        .exports
        .iter()
        .filter(|export| reachable.contains_module_symbol(&module.path, &export.exported_name))
        .map(|export| export.local_name.clone())
        .collect::<HashSet<_>>();
    let mut pending = retained.iter().cloned().collect::<Vec<_>>();

    while let Some(binding_name) = pending.pop() {
        for dependency in module
            .binding_dependencies
            .iter()
            .filter(|dependency| dependency.binding_name == binding_name)
        {
            if retained.insert(dependency.referenced_name.clone()) {
                pending.push(dependency.referenced_name.clone());
            }
        }
    }

    retained
}

fn module_has_reachable_export(module: &ModuleRecord, reachable: &ReachableExports) -> bool {
    module
        .exports
        .iter()
        .any(|export| reachable.contains_module_symbol(&module.path, &export.exported_name))
}

fn retained_import_names(
    import: &crate::pipeline::graph::ImportEdge,
    include_all_static_imports: bool,
    retained_bindings: &HashSet<String>,
) -> Vec<String> {
    if import.imported_names.is_empty() {
        return Vec::new();
    }
    if include_all_static_imports {
        return import.imported_names.clone();
    }

    let mut names = import
        .imported_bindings
        .iter()
        .filter(|binding| retained_bindings.contains(&binding.local_name))
        .map(|binding| binding.imported_name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn mark_reachable_import_target(
    graph: &ModuleGraph,
    target_id: ModuleId,
    retained_import_names: &[String],
    target_keep_all: bool,
    reachable: &mut ReachableExports,
) {
    let Some(target) = graph.module_by_id(target_id) else {
        return;
    };

    if target_keep_all {
        reachable.mark_full_module(target.path.clone());
    } else if retained_import_names.is_empty() {
        reachable.mark_module(target.path.clone());
    } else {
        for imported_name in retained_import_names {
            reachable.mark_module_symbol(target.path.clone(), imported_name.clone());
        }
    }
}

fn rewrite_module(
    module: &ModuleRecord,
    reachable: &ReachableExports,
    keep_all_exports: bool,
    renames: &HashMap<String, String>,
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

    let rename_replacements = semantic_rename_replacements(module, renames, &replacements)?;
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
    external_indexes: &HashMap<String, usize>,
) -> Result<HashMap<String, String>, String> {
    let mut renames = HashMap::new();

    for binding in &module.local_bindings {
        renames.insert(binding.clone(), module_binding_name(module.id, binding));
    }

    for ext in &module.external_imports {
        if ext.local_name.is_empty() {
            continue;
        }
        if let Some(index) = external_indexes.get(&ext.specifier) {
            renames.insert(
                ext.local_name.clone(),
                external_binding_name(*index, &ext.imported_name),
            );
        }
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

            let target_name = resolve_export_binding(
                graph,
                target_id,
                &binding.imported_name,
                &mut HashSet::new(),
            )
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
    visited: &mut HashSet<(ModuleId, String)>,
) -> Option<String> {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return None;
    }

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
            && let Some(binding) =
                resolve_export_binding(graph, target_id, &reexport.imported_name, visited)
        {
            return Some(binding);
        }
    }

    for star_export in &module.star_exports {
        let target_id = graph.module_id_by_path(&star_export.resolved_path)?;
        if let Some(binding) = resolve_export_binding(graph, target_id, exported_name, visited) {
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

    // Spans were recorded once at graph build (module.root_symbol_spans /
    // module.shorthand_spans); reuse them instead of re-parsing and re-running
    // semantic analysis on every bundle request.
    let shorthand_spans: HashSet<(usize, usize)> =
        module.shorthand_spans.iter().copied().collect();
    let mut replacements = Vec::new();
    let mut seen_spans = HashSet::new();

    for symbol in &module.root_symbol_spans {
        let Some(new_name) = renames.get(&symbol.name) else {
            continue;
        };

        for &span in std::iter::once(&symbol.decl).chain(symbol.references.iter()) {
            push_semantic_rename(
                &module.source,
                span,
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
    (start, end): (usize, usize),
    new_name: &str,
    shorthand_spans: &HashSet<(usize, usize)>,
    protected_replacements: &[Replacement],
    seen_spans: &mut HashSet<(usize, usize)>,
    replacements: &mut Vec<Replacement>,
) -> Result<(), String> {
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
    renames: &HashMap<String, String>,
) -> String {
    let mut markers = String::new();
    for export in &module.exports {
        if keep_all_exports || reachable.contains_module_symbol(&module.path, &export.exported_name)
        {
            let local_binding = renames
                .get(&export.local_name)
                .cloned()
                .unwrap_or_else(|| module_binding_name(module.id, &export.local_name));
            markers.push_str("export { ");
            markers.push_str(&local_binding);
            markers.push_str(" as __importLensUse_");
            markers.push_str(&module_binding_name(module.id, &export.exported_name));
            markers.push_str(" };\n");
        }
    }
    markers
}

fn module_binding_name(module_id: ModuleId, name: &str) -> String {
    format!("__il_m{}_{}", module_id.0, sanitize_identifier(name))
}

fn external_binding_name(index: usize, imported_name: &str) -> String {
    let suffix = match imported_name {
        "default" => "default".to_owned(),
        "*" => "ns".to_owned(),
        name => sanitize_identifier(name),
    };
    format!("__il_ext{index}_{suffix}")
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
