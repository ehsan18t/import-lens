use crate::{
    ipc::protocol::ModuleContribution,
    pipeline::{
        graph::{
            ExternalImportEdge, ModuleGraph, ModuleId, ModuleRecord, module_exported_names,
            module_provides_export,
        },
        reachability::ReachableExports,
        replacements::{Replacement, apply_replacements, span_overlaps_replacements},
        util::{is_identifier_continue, is_identifier_start},
    },
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
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
        let rewritten = rewrite_module(
            graph,
            module,
            &expanded_reachable,
            keep_all_exports,
            &renames,
        )?;
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

    // A namespace import renames its local binding to `__il_m<N>_namespace`;
    // without this declaration the bundle reads an identifier no module defines.
    for target_id in namespace_target_ids(graph, &included) {
        let Some(declaration) = namespace_object_declaration(graph, target_id, &included) else {
            continue;
        };
        let Some(target) = graph.module_by_id(target_id) else {
            continue;
        };
        // The target usually already contributed its rewritten source. Fold the
        // declaration into that row: a second row for the same path would make
        // the module breakdown list one file twice and evict a real entry.
        let path = target.path.to_string_lossy().to_string();
        let bytes = declaration.len() as u64;
        match contributions
            .iter_mut()
            .find(|contribution| contribution.path == path)
        {
            Some(contribution) => contribution.bytes += bytes,
            None => contributions.push(ModuleContribution { path, bytes }),
        }
        source.push_str(&declaration);
        minifier_source.push_str(&declaration);
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
    // A newly reachable re-export adds no local binding, so keying the re-visit
    // guard on bindings alone would early-return and never follow its chain.
    let mut change_key = retained_bindings.clone();
    change_key.extend(reachable.module_symbol_names(&module.path));
    if previous_keep_all == Some(next_keep_all)
        && !retained_bindings_changed(module_id, next_keep_all, &change_key, processed_bindings)
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
            let retained_import_names = retained_import_names(
                graph,
                module,
                import,
                target_id,
                include_all_static_imports,
                &retained_bindings,
            );
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

    // Re-export and star edges carry a reachable symbol to the module that
    // defines it. Following them only under `next_keep_all` would drop that
    // definition whenever the symbol arrives through a plain named import.
    for reexport in &module.reexports {
        let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path) else {
            continue;
        };
        if !next_keep_all
            && !reachable.contains_module_symbol(&module.path, &reexport.exported_name)
        {
            continue;
        }
        let target_keep_all = next_keep_all || reexport.imported_name == "*";
        if let Some(target) = graph.module_by_id(target_id) {
            if target_keep_all {
                reachable.mark_full_module(target.path.clone());
            } else {
                reachable.mark_module_symbol(target.path.clone(), reexport.imported_name.clone());
            }
        }
        include_module_with_imports(
            graph,
            target_id,
            target_keep_all,
            reachable,
            included,
            processed_bindings,
        );
    }
    for star_export in &module.star_exports {
        let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path) else {
            continue;
        };
        if next_keep_all {
            include_module_with_imports(
                graph,
                target_id,
                true,
                reachable,
                included,
                processed_bindings,
            );
            continue;
        }
        let mut followed_any = false;
        for name in reachable.module_symbol_names(&module.path) {
            if module_provides_export(graph, target_id, &name, &mut HashSet::new()) {
                if let Some(target) = graph.module_by_id(target_id) {
                    reachable.mark_module_symbol(target.path.clone(), name.clone());
                }
                followed_any = true;
            }
        }
        if followed_any {
            include_module_with_imports(
                graph,
                target_id,
                false,
                reachable,
                included,
                processed_bindings,
            );
        }
    }
}

fn retained_bindings_changed(
    module_id: ModuleId,
    keep_all_exports: bool,
    change_key: &HashSet<String>,
    processed_bindings: &mut HashMap<ModuleId, HashSet<String>>,
) -> bool {
    if keep_all_exports {
        return false;
    }

    let processed = processed_bindings.entry(module_id).or_default();
    let changed = change_key.iter().any(|name| !processed.contains(name));
    processed.extend(change_key.iter().cloned());
    changed
}

fn retained_binding_names(module: &ModuleRecord, reachable: &ReachableExports) -> HashSet<String> {
    let mut retained = module
        .exports
        .iter()
        .filter(|export| reachable.contains_module_symbol(&module.path, &export.exported_name))
        .map(|export| export.local_name.clone())
        .collect::<HashSet<_>>();
    // A top-level statement that binds nothing (`setup(dep);`) survives
    // rewriting and the minifier cannot prove it side-effect free, so whatever
    // it reads is a retention root. Otherwise an import used only by such a
    // statement is pruned and the bundle references an undeclared binding.
    retained.extend(module.side_effect_references.iter().cloned());
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

/// Whether this module contributes any reachable export. Reachability records
/// re-exported and star-passed names against the re-exporting module's own
/// path, so asking by path covers local exports, named re-exports and star
/// pass-throughs alike. A module reached purely for side effects never gets a
/// symbol, so it keeps the conservative "I cannot tell which imports matter,
/// keep them all" fallback.
fn module_has_reachable_export(module: &ModuleRecord, reachable: &ReachableExports) -> bool {
    reachable.has_module_symbols(&module.path)
}

fn retained_import_names(
    graph: &ModuleGraph,
    module: &ModuleRecord,
    import: &crate::pipeline::graph::ImportEdge,
    target_id: ModuleId,
    include_all_static_imports: bool,
    retained_bindings: &HashSet<String>,
) -> Vec<String> {
    if import.imported_names.is_empty() {
        return Vec::new();
    }
    if include_all_static_imports {
        return import.imported_names.clone();
    }

    let mut names = Vec::new();
    for binding in &import.imported_bindings {
        if binding.imported_name == "*" {
            // A namespace read only through `ns.prop` retains just those props;
            // "*" would force the whole target into the bundle.
            match classify_namespace_use(graph, module, &binding.local_name, target_id) {
                NamespaceUse::Inlined(properties) => names.extend(properties),
                NamespaceUse::Escaping => names.push("*".to_owned()),
            }
            continue;
        }
        if retained_bindings.contains(&binding.local_name) {
            names.push(binding.imported_name.clone());
        }
    }
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
    graph: &ModuleGraph,
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

    // Must precede the rename pass: each `ns.alpha` span contains the `ns`
    // identifier span, and `semantic_rename_replacements` skips any rename that
    // overlaps an already-planned replacement.
    replacements.extend(namespace_member_replacements(graph, module));

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

/// Strip `keyword` from the front of `text` only when it appears as a whole
/// word, i.e. the following byte is not an identifier continuation. This keeps
/// identifiers such as `functionRegistry` or `classNames` from being mistaken
/// for the `function`/`class` keywords.
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(keyword)?;
    match rest.bytes().next() {
        Some(next) if is_identifier_continue(next) => None,
        _ => Some(rest),
    }
}

/// An `export default` declaration is *named* only when a binding identifier
/// follows the (optionally `async`-prefixed, optionally `*`-suffixed) keyword.
/// Anonymous forms (`class {}`, `class extends X {}`, `function () {}`,
/// `function* () {}`, and their `async` variants) and plain expressions must be
/// wrapped as `const <default binding> = <expr>` instead of being left as a
/// nameless declaration statement, which is a syntax error.
fn is_named_default_declaration(trimmed_after_default: &str) -> bool {
    let rest = strip_keyword(trimmed_after_default, "async")
        .map(str::trim_start)
        .unwrap_or(trimmed_after_default);

    if let Some(after_function) = strip_keyword(rest, "function") {
        let after_star = after_function
            .trim_start()
            .strip_prefix('*')
            .unwrap_or(after_function);
        return after_star
            .trim_start()
            .bytes()
            .next()
            .is_some_and(is_identifier_start);
    }

    if let Some(after_class) = strip_keyword(rest, "class") {
        let next = after_class.trim_start();
        // `class Foo ...` is named; `class {` and `class extends ...` are
        // anonymous. `extends` must be a whole word so a class literally named
        // `extendsFoo` is still treated as a named declaration.
        return next.bytes().next().is_some_and(is_identifier_start)
            && strip_keyword(next, "extends").is_none();
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
                // An inlined namespace has no binding of its own: every
                // `ns.prop` is replaced whole by `namespace_member_replacements`,
                // and those spans shield the inner `ns` from being renamed.
                if matches!(
                    classify_namespace_use(graph, module, &binding.local_name, target_id),
                    NamespaceUse::Escaping
                ) {
                    renames.insert(
                        binding.local_name.clone(),
                        module_binding_name(target_id, "namespace"),
                    );
                }
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

/// How a single `* as ns` binding is used inside one module.
enum NamespaceUse {
    /// Every root-scope reference to `ns` is the object of a static member
    /// access whose property resolves to an export of the target, so each
    /// `ns.prop` can be rewritten to that export's binding and the rest of the
    /// target tree-shakes. Carries the accessed names, sorted and deduplicated.
    Inlined(Vec<String>),
    /// `ns` is read as a value, accessed computedly, optionally chained, or
    /// reads a name the target does not export. Keep the target whole and
    /// materialize its namespace object.
    Escaping,
}

fn classify_namespace_use(
    graph: &ModuleGraph,
    module: &ModuleRecord,
    local_name: &str,
    target_id: ModuleId,
) -> NamespaceUse {
    let Some(symbol) = module
        .root_symbol_spans
        .iter()
        .find(|symbol| symbol.name == local_name)
    else {
        // No root-scope references at all: nothing reads the namespace, so
        // there is nothing to inline and nothing to materialize.
        return NamespaceUse::Inlined(Vec::new());
    };

    let accesses_by_object = module
        .static_member_accesses
        .iter()
        .map(|access| (access.object, access))
        .collect::<HashMap<_, _>>();

    let mut properties = Vec::new();
    for reference in &symbol.references {
        // A reference that is not the object of a static access is the
        // namespace itself escaping into a value position.
        let Some(access) = accesses_by_object.get(reference) else {
            return NamespaceUse::Escaping;
        };
        if resolve_export_binding(graph, target_id, &access.property, &mut HashSet::new()).is_none()
        {
            return NamespaceUse::Escaping;
        }
        properties.push(access.property.clone());
    }

    properties.sort();
    properties.dedup();
    NamespaceUse::Inlined(properties)
}

/// Modules that some included module imports as `* as ns` in a way that escapes.
/// A `BTreeSet` keeps emission order deterministic, so bundle bytes never depend
/// on `HashMap` iteration order.
fn namespace_target_ids(
    graph: &ModuleGraph,
    included: &HashMap<ModuleId, bool>,
) -> BTreeSet<ModuleId> {
    let mut targets = BTreeSet::new();

    for module in graph
        .modules
        .iter()
        .filter(|module| included.contains_key(&module.id))
    {
        for import in &module.imports {
            let Some(target_id) = graph.module_id_by_path(&import.resolved_path) else {
                continue;
            };
            if !included.contains_key(&target_id) {
                continue;
            }
            for binding in &import.imported_bindings {
                if binding.imported_name == "*" {
                    match classify_namespace_use(graph, module, &binding.local_name, target_id) {
                        NamespaceUse::Escaping => {
                            targets.insert(target_id);
                        }
                        // An inlined `ns.AnPlusB` rewrites to AnPlusB's
                        // namespace object when the barrel re-exports it as one.
                        NamespaceUse::Inlined(properties) => {
                            for property in properties {
                                if let Some(owner) = namespace_owner(graph, target_id, &property)
                                    && included.contains_key(&owner)
                                {
                                    targets.insert(owner);
                                }
                            }
                        }
                    }
                    continue;
                }

                // `import { X } from "./barrel.js"` where barrel says
                // `export * as X from "./x.js"` binds X to x's namespace.
                if let Some(owner) = namespace_owner(graph, target_id, &binding.imported_name)
                    && included.contains_key(&owner)
                {
                    targets.insert(owner);
                }
            }
        }
    }

    // A materialized barrel's object literal names its children's namespaces, so
    // those must be declared too. Walk exactly the members
    // `namespace_object_declaration` will emit -- deriving them any other way
    // (say, from `reexports` alone) misses a namespace re-export forwarded
    // through `export *`, and the declaration then names an undeclared child.
    // Seeding from demand rather than from every `export * as` in the graph also
    // keeps a dead re-export from dragging a namespace and its members in.
    let mut pending = targets.iter().copied().collect::<Vec<_>>();
    while let Some(target_id) = pending.pop() {
        for exported_name in module_exported_names(graph, target_id, true) {
            if let Some(child_id) = namespace_owner(graph, target_id, &exported_name)
                && included.contains_key(&child_id)
                && targets.insert(child_id)
            {
                pending.push(child_id);
            }
        }
    }

    targets
}

/// `Some(owner)` when `exported_name` resolves to a module namespace object
/// rather than a plain binding, i.e. it arrives through `export * as`.
fn namespace_owner(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
) -> Option<ModuleId> {
    let (owner, binding) =
        resolve_export_binding_owner(graph, module_id, exported_name, &mut HashSet::new())?;
    (binding == module_binding_name(owner, "namespace")).then_some(owner)
}

/// `const __il_m1_namespace = { alpha: __il_m1_alpha, beta: __il_m1_beta };`
///
/// esbuild emits live getters through an `__export` helper. A plain object
/// literal is a few bytes smaller and keeps every member binding alive for the
/// minifier, which is all a size estimate needs -- the bundle is measured,
/// never executed.
fn namespace_object_declaration(
    graph: &ModuleGraph,
    target_id: ModuleId,
    included: &HashMap<ModuleId, bool>,
) -> Option<String> {
    let members = module_exported_names(graph, target_id, true)
        .iter()
        .filter_map(|name| {
            let (owner, binding) =
                resolve_export_binding_owner(graph, target_id, name, &mut HashSet::new())?;
            // Never name a binding from a module the bundle excluded: that would
            // trade one dangling reference for another.
            included
                .contains_key(&owner)
                .then(|| format!("{}: {binding}", namespace_member_key(name)))
        })
        .collect::<Vec<_>>();
    if members.is_empty() {
        return None;
    }

    Some(format!(
        "const {} = {{ {} }};\n",
        module_binding_name(target_id, "namespace"),
        members.join(", ")
    ))
}

/// An export name is not necessarily a bare identifier -- `export { x as "a-b" }`
/// and non-ASCII names are both legal -- so anything the ASCII identifier
/// predicates reject is emitted as a quoted key rather than a syntax error.
fn namespace_member_key(name: &str) -> String {
    let mut bytes = name.bytes();
    let is_identifier = bytes.next().is_some_and(is_identifier_start)
        && bytes.all(is_identifier_continue)
        && !name.is_empty();

    if is_identifier {
        return name.to_owned();
    }

    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn resolve_export_binding(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> Option<String> {
    resolve_export_binding_owner(graph, module_id, exported_name, visited)
        .map(|(_, binding)| binding)
}

/// Resolves an exported name to the module that declares its binding and the
/// bundle-local name of that binding, following re-export and star-export
/// chains. Callers that must know whether the declaring module is in the bundle
/// use the `ModuleId`; the rest take only the name.
fn resolve_export_binding_owner(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> Option<(ModuleId, String)> {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return None;
    }

    let module = graph.module_by_id(module_id)?;
    if let Some(export) = module
        .exports
        .iter()
        .find(|export| export.exported_name == exported_name)
    {
        return Some((
            module_id,
            module_binding_name(module_id, &export.local_name),
        ));
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path) else {
            continue;
        };

        // `export * as X from "./x.js"` re-exports x's whole namespace under the
        // name X. There is no export literally called `*` to recurse into; the
        // binding is x's namespace object, which `namespace_target_ids` makes
        // sure gets declared.
        if reexport.imported_name == "*" {
            return Some((target_id, module_binding_name(target_id, "namespace")));
        }

        if let Some(resolved) =
            resolve_export_binding_owner(graph, target_id, &reexport.imported_name, visited)
        {
            return Some(resolved);
        }
    }

    for star_export in &module.star_exports {
        let target_id = graph.module_id_by_path(&star_export.resolved_path)?;
        if let Some(resolved) =
            resolve_export_binding_owner(graph, target_id, exported_name, visited)
        {
            return Some(resolved);
        }
    }

    None
}

/// `ns.alpha` -> `__il_m1_alpha`, for namespace imports classified as `Inlined`.
/// The returned spans cover the whole member expression, so feeding them to
/// `semantic_rename_replacements` as protected spans is what stops the inner
/// `ns` identifier from also being renamed.
fn namespace_member_replacements(graph: &ModuleGraph, module: &ModuleRecord) -> Vec<Replacement> {
    let mut replacements = Vec::new();
    for import in &module.imports {
        let Some(target_id) = graph.module_id_by_path(&import.resolved_path) else {
            continue;
        };
        for binding in &import.imported_bindings {
            if binding.imported_name != "*" {
                continue;
            }
            if matches!(
                classify_namespace_use(graph, module, &binding.local_name, target_id),
                NamespaceUse::Escaping
            ) {
                continue;
            }
            let Some(symbol) = module
                .root_symbol_spans
                .iter()
                .find(|symbol| symbol.name == binding.local_name)
            else {
                continue;
            };
            let references = symbol.references.iter().copied().collect::<HashSet<_>>();

            for access in &module.static_member_accesses {
                // Only accesses whose object is this root-scope binding; a
                // shadowed `ns` in a nested scope has a different span.
                if !references.contains(&access.object) {
                    continue;
                }
                let Some(target_name) =
                    resolve_export_binding(graph, target_id, &access.property, &mut HashSet::new())
                else {
                    continue;
                };
                replacements.push(Replacement::replace(
                    access.span.0,
                    access.span.1,
                    target_name,
                ));
            }
        }
    }
    replacements
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
    let shorthand_spans: HashSet<(usize, usize)> = module.shorthand_spans.iter().copied().collect();
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
    let mut lossy = name.is_empty();
    for (index, byte) in name.bytes().enumerate() {
        let valid = if index == 0 {
            is_identifier_start(byte)
        } else {
            is_identifier_continue(byte)
        };
        if valid {
            sanitized.push(byte as char);
        } else {
            sanitized.push('_');
            lossy = true;
        }
    }
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    if lossy {
        // Distinct identifiers that lose information to '_' replacement (e.g.
        // two non-ASCII names differing only in replaced bytes) would otherwise
        // collide into one binding. Append a short deterministic hash of the
        // original so they stay distinct. Pure-ASCII identifiers are untouched.
        use std::fmt::Write as _;
        let _ = write!(sanitized, "_{:08x}", fnv1a_32(name));
    }
    sanitized
}

fn fnv1a_32(value: &str) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}
