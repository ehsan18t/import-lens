use crate::pipeline::resolver::append_extension;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingPattern, Class, ClassElement, Declaration, ExportDefaultDeclarationKind, Expression,
    Program, Statement,
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};
use oxc_syntax::module_record::{
    ExportEntry, ExportExportName, ExportImportName, ExportLocalName, ImportEntry,
    ImportImportName, ModuleRecord as OxcModuleRecord,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

pub const MAX_GRAPH_MODULES: usize = 2_000;
pub const MAX_MODULE_SOURCE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_GRAPH_SOURCE_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphLimits {
    pub max_modules: usize,
    pub max_module_source_bytes: usize,
    pub max_graph_source_bytes: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            max_modules: MAX_GRAPH_MODULES,
            max_module_source_bytes: MAX_MODULE_SOURCE_BYTES,
            max_graph_source_bytes: MAX_GRAPH_SOURCE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone)]
pub struct ModuleRecord {
    pub id: ModuleId,
    pub path: PathBuf,
    pub source: String,
    pub imports: Vec<ImportEdge>,
    pub external_imports: Vec<ExternalImportEdge>,
    pub import_statement_spans: Vec<(usize, usize)>,
    pub export_specifier_statement_spans: Vec<(usize, usize)>,
    pub exports: Vec<ExportRecord>,
    pub reexports: Vec<ReExportRecord>,
    pub star_exports: Vec<StarExportRecord>,
    pub local_bindings: Vec<String>,
    pub has_top_level_side_effects: bool,
}

#[derive(Debug, Clone)]
pub struct ImportEdge {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_names: Vec<String>,
    pub imported_bindings: Vec<ImportedBinding>,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct ExternalImportEdge {
    pub specifier: String,
    pub imported_name: String,
    pub local_name: String,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct ImportedBinding {
    pub imported_name: String,
    pub local_name: String,
}

#[derive(Debug, Clone)]
pub struct ExportRecord {
    pub exported_name: String,
    pub local_name: String,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct ReExportRecord {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_name: String,
    pub exported_name: String,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct StarExportRecord {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub statement_start: usize,
    pub statement_end: usize,
}

#[derive(Debug, Clone)]
pub struct ModuleGraph {
    pub entry_id: ModuleId,
    pub modules: Vec<ModuleRecord>,
    path_to_id: HashMap<PathBuf, ModuleId>,
}

impl Default for ModuleGraph {
    fn default() -> Self {
        Self {
            entry_id: ModuleId(0),
            modules: Vec::new(),
            path_to_id: HashMap::new(),
        }
    }
}

impl ModuleGraph {
    pub fn entry_module(&self) -> Option<&ModuleRecord> {
        self.module_by_id(self.entry_id)
    }

    pub fn module_by_id(&self, id: ModuleId) -> Option<&ModuleRecord> {
        self.modules.get(id.0)
    }

    pub fn module_id_by_path(&self, path: &Path) -> Option<ModuleId> {
        self.path_to_id.get(path).copied()
    }
}

pub fn build_module_graph(entry_path: &Path) -> Result<ModuleGraph, String> {
    build_module_graph_with_limits(entry_path, GraphLimits::default())
}

pub fn build_module_graph_with_limits(
    entry_path: &Path,
    limits: GraphLimits,
) -> Result<ModuleGraph, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    let mut builder = ModuleGraphBuilder::new(limits);
    let entry_id = builder.load_module(&entry_path)?;
    builder.graph.entry_id = entry_id;

    Ok(builder.graph)
}

struct ModuleGraphBuilder {
    graph: ModuleGraph,
    limits: GraphLimits,
    graph_source_bytes: usize,
}

impl ModuleGraphBuilder {
    fn new(limits: GraphLimits) -> Self {
        Self {
            graph: ModuleGraph::default(),
            limits,
            graph_source_bytes: 0,
        }
    }
}

impl ModuleGraphBuilder {
    fn load_module(&mut self, path: &Path) -> Result<ModuleId, String> {
        let path = normalize_existing_path(path)?;
        if let Some(existing) = self.graph.path_to_id.get(&path) {
            return Ok(*existing);
        }
        if self.graph.modules.len() >= self.limits.max_modules {
            return Err(format!(
                "module count limit exceeded while loading {}; limit: {}",
                path.display(),
                self.limits.max_modules
            ));
        }

        let source = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read module {}: {error}", path.display()))?;
        let source_bytes = source.len();
        if source_bytes > self.limits.max_module_source_bytes {
            return Err(format!(
                "module source size {} exceeds limit {} in {}",
                source_bytes,
                self.limits.max_module_source_bytes,
                path.display()
            ));
        }
        let next_graph_source_bytes = self
            .graph_source_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| {
                format!(
                    "graph source size overflow while loading {}",
                    path.display()
                )
            })?;
        if next_graph_source_bytes > self.limits.max_graph_source_bytes {
            return Err(format!(
                "graph source size {} exceeds limit {} while loading {}",
                next_graph_source_bytes,
                self.limits.max_graph_source_bytes,
                path.display()
            ));
        }

        let parsed = parse_module(&path, &source)?;
        let id = ModuleId(self.graph.modules.len());
        let next_paths = parsed
            .imports
            .iter()
            .map(|edge| edge.resolved_path.clone())
            .chain(
                parsed
                    .reexports
                    .iter()
                    .map(|edge| edge.resolved_path.clone()),
            )
            .chain(
                parsed
                    .star_exports
                    .iter()
                    .map(|edge| edge.resolved_path.clone()),
            )
            .collect::<Vec<_>>();

        self.graph.path_to_id.insert(path.clone(), id);
        self.graph_source_bytes = next_graph_source_bytes;
        self.graph.modules.push(ModuleRecord {
            id,
            path,
            source,
            imports: parsed.imports,
            external_imports: parsed.external_imports,
            import_statement_spans: parsed.import_statement_spans,
            export_specifier_statement_spans: parsed.export_specifier_statement_spans,
            exports: parsed.exports,
            reexports: parsed.reexports,
            star_exports: parsed.star_exports,
            local_bindings: parsed.local_bindings,
            has_top_level_side_effects: parsed.has_top_level_side_effects,
        });

        for next_path in next_paths {
            self.load_module(&next_path)?;
        }

        Ok(id)
    }
}

#[derive(Debug, Default)]
struct ParsedModule {
    imports: Vec<ImportEdge>,
    external_imports: Vec<ExternalImportEdge>,
    import_statement_spans: Vec<(usize, usize)>,
    export_specifier_statement_spans: Vec<(usize, usize)>,
    exports: Vec<ExportRecord>,
    reexports: Vec<ReExportRecord>,
    star_exports: Vec<StarExportRecord>,
    local_bindings: Vec<String>,
    has_top_level_side_effects: bool,
}

fn parse_module(path: &Path, source: &str) -> Result<ParsedModule, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "failed to parse module {}; errors: {}",
            path.display(),
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
            "semantic validation failed for {}; errors: {}",
            path.display(),
            semantic
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let edges_result = import_edges(path, &parsed.module_record)?;
    Ok(ParsedModule {
        imports: edges_result.imports,
        external_imports: edges_result.external_imports,
        import_statement_spans: edges_result.import_statement_spans,
        export_specifier_statement_spans: export_specifier_statement_spans(&parsed.program),
        exports: export_records(&parsed.module_record),
        reexports: reexport_records(path, &parsed.module_record)?,
        star_exports: star_export_records(path, &parsed.module_record)?,
        local_bindings: local_bindings(&parsed.program),
        has_top_level_side_effects: has_top_level_side_effects(&parsed.program),
    })
}

struct ImportEdgesResult {
    imports: Vec<ImportEdge>,
    external_imports: Vec<ExternalImportEdge>,
    import_statement_spans: Vec<(usize, usize)>,
}

fn import_edges(
    path: &Path,
    module_record: &OxcModuleRecord<'_>,
) -> Result<ImportEdgesResult, String> {
    let mut imports = Vec::new();
    let mut external_imports = Vec::new();
    let mut import_statement_spans = Vec::new();
    let mut binding_import_specifiers = HashSet::new();

    for requested_modules in module_record.requested_modules.values() {
        for request in requested_modules {
            if request.is_import && !request.is_type {
                let span = (
                    span_start(request.statement_span),
                    span_end(request.statement_span),
                );
                if !import_statement_spans.contains(&span) {
                    import_statement_spans.push(span);
                }
            }
        }
    }

    for entry in module_record
        .import_entries
        .iter()
        .filter(|entry| !entry.is_type)
    {
        let specifier = entry.module_request.name.as_str().to_owned();
        binding_import_specifiers.insert(specifier.clone());
        let imported_name = import_name(entry);
        let local_name = entry.local_name.name.as_str().to_owned();
        if let Some(resolved_path) = resolve_relative_module(path, &specifier)? {
            push_import_binding(
                &mut imports,
                specifier,
                resolved_path,
                imported_name,
                local_name,
                entry.statement_span,
            );
        } else {
            external_imports.push(ExternalImportEdge {
                specifier,
                imported_name,
                local_name,
                statement_start: span_start(entry.statement_span),
                statement_end: span_end(entry.statement_span),
            });
        }
    }

    for (specifier, requested_modules) in &module_record.requested_modules {
        if binding_import_specifiers.contains(specifier.as_str()) {
            continue;
        }

        let is_side_effect_import = requested_modules
            .iter()
            .any(|request| request.is_import && !request.is_type);
        if !is_side_effect_import {
            continue;
        }

        let statement_span = requested_modules[0].statement_span;
        if let Some(resolved_path) = resolve_relative_module(path, specifier.as_str())? {
            imports.push(ImportEdge {
                specifier: specifier.as_str().to_owned(),
                resolved_path,
                imported_names: Vec::new(),
                imported_bindings: Vec::new(),
                statement_start: span_start(statement_span),
                statement_end: span_end(statement_span),
            });
        } else {
            external_imports.push(ExternalImportEdge {
                specifier: specifier.as_str().to_owned(),
                imported_name: String::new(),
                local_name: String::new(),
                statement_start: span_start(statement_span),
                statement_end: span_end(statement_span),
            });
        }
    }

    Ok(ImportEdgesResult {
        imports,
        external_imports,
        import_statement_spans,
    })
}

fn push_import_binding(
    imports: &mut Vec<ImportEdge>,
    specifier: String,
    resolved_path: PathBuf,
    imported_name: String,
    local_name: String,
    statement_span: Span,
) {
    if let Some(edge) = imports
        .iter_mut()
        .find(|edge| edge.specifier == specifier && edge.resolved_path == resolved_path)
    {
        if !edge.imported_names.contains(&imported_name) {
            edge.imported_names.push(imported_name.clone());
        }
        if !edge
            .imported_bindings
            .iter()
            .any(|binding| binding.local_name == local_name)
        {
            edge.imported_bindings.push(ImportedBinding {
                imported_name,
                local_name,
            });
        }
        return;
    }

    imports.push(ImportEdge {
        specifier,
        resolved_path,
        imported_names: vec![imported_name.clone()],
        imported_bindings: vec![ImportedBinding {
            imported_name,
            local_name,
        }],
        statement_start: span_start(statement_span),
        statement_end: span_end(statement_span),
    });
}

fn import_name(entry: &ImportEntry<'_>) -> String {
    match &entry.import_name {
        ImportImportName::Name(name) => name.name.as_str().to_owned(),
        ImportImportName::NamespaceObject => "*".to_owned(),
        ImportImportName::Default(_) => "default".to_owned(),
    }
}

fn export_records(module_record: &OxcModuleRecord<'_>) -> Vec<ExportRecord> {
    module_record
        .local_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| {
            let exported_name = export_export_name(&entry.export_name)?;
            let local_name =
                export_local_name(&entry.local_name).unwrap_or_else(|| exported_name.clone());
            Some(ExportRecord {
                exported_name,
                local_name,
                statement_start: span_start(entry.statement_span),
                statement_end: span_end(entry.statement_span),
            })
        })
        .collect()
}

fn reexport_records(
    path: &Path,
    module_record: &OxcModuleRecord<'_>,
) -> Result<Vec<ReExportRecord>, String> {
    module_record
        .indirect_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| reexport_record(path, entry).transpose())
        .collect()
}

fn reexport_record(path: &Path, entry: &ExportEntry<'_>) -> Result<Option<ReExportRecord>, String> {
    let Some(module_request) = &entry.module_request else {
        return Ok(None);
    };
    let Some(resolved_path) = resolve_relative_module(path, module_request.name.as_str())? else {
        return Ok(None);
    };
    let Some(exported_name) = export_export_name(&entry.export_name) else {
        return Ok(None);
    };
    let Some(imported_name) = export_import_name(&entry.import_name) else {
        return Ok(None);
    };

    Ok(Some(ReExportRecord {
        specifier: module_request.name.as_str().to_owned(),
        resolved_path,
        imported_name,
        exported_name,
        statement_start: span_start(entry.statement_span),
        statement_end: span_end(entry.statement_span),
    }))
}

fn star_export_records(
    path: &Path,
    module_record: &OxcModuleRecord<'_>,
) -> Result<Vec<StarExportRecord>, String> {
    module_record
        .star_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| {
            let module_request = entry.module_request.as_ref()?;
            let resolved_path =
                resolve_relative_module(path, module_request.name.as_str()).transpose()?;
            Some(resolved_path.map(|resolved_path| StarExportRecord {
                specifier: module_request.name.as_str().to_owned(),
                resolved_path,
                statement_start: span_start(entry.statement_span),
                statement_end: span_end(entry.statement_span),
            }))
        })
        .collect()
}

fn export_import_name(name: &ExportImportName<'_>) -> Option<String> {
    match name {
        ExportImportName::Name(name) => Some(name.name.as_str().to_owned()),
        ExportImportName::All => Some("*".to_owned()),
        ExportImportName::AllButDefault | ExportImportName::Null => None,
    }
}

fn export_export_name(name: &ExportExportName<'_>) -> Option<String> {
    match name {
        ExportExportName::Name(name) => Some(name.name.as_str().to_owned()),
        ExportExportName::Default(_) => Some("default".to_owned()),
        ExportExportName::Null => None,
    }
}

fn export_local_name(name: &ExportLocalName<'_>) -> Option<String> {
    match name {
        ExportLocalName::Name(name) | ExportLocalName::Default(name) => {
            Some(name.name.as_str().to_owned())
        }
        ExportLocalName::Null => None,
    }
}

fn export_specifier_statement_spans(program: &Program<'_>) -> Vec<(usize, usize)> {
    let mut spans = program
        .body
        .iter()
        .filter_map(|statement| match statement {
            Statement::ExportNamedDeclaration(export) if export.declaration.is_none() => {
                Some((span_start(export.span), span_end(export.span)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    spans.sort();
    spans.dedup();
    spans
}

fn local_bindings(program: &Program<'_>) -> Vec<String> {
    let mut bindings = Vec::new();
    for statement in &program.body {
        collect_statement_bindings(statement, &mut bindings);
    }
    bindings.sort();
    bindings.dedup();
    bindings
}

fn collect_statement_bindings(statement: &Statement<'_>, bindings: &mut Vec<String>) {
    match statement {
        Statement::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                collect_binding_pattern(&declarator.id, bindings);
            }
        }
        Statement::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(declaration) = &export.declaration {
                collect_declaration_bindings(declaration, bindings);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            collect_export_default_bindings(&export.declaration, bindings);
        }
        _ => {}
    }
}

fn collect_declaration_bindings(declaration: &Declaration<'_>, bindings: &mut Vec<String>) {
    match declaration {
        Declaration::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                collect_binding_pattern(&declarator.id, bindings);
            }
        }
        Declaration::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.push(id.name.as_str().to_owned());
            }
        }
        _ => {}
    }
}

fn collect_export_default_bindings(
    declaration: &ExportDefaultDeclarationKind<'_>,
    bindings: &mut Vec<String>,
) {
    match declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                bindings.push(id.name.as_str().to_owned());
            } else {
                bindings.push("default".to_owned());
            }
        }
        ExportDefaultDeclarationKind::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                bindings.push(id.name.as_str().to_owned());
            } else {
                bindings.push("default".to_owned());
            }
        }
        _ => bindings.push("default".to_owned()),
    }
}

fn collect_binding_pattern(pattern: &BindingPattern<'_>, bindings: &mut Vec<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            bindings.push(identifier.name.as_str().to_owned());
        }
        BindingPattern::AssignmentPattern(assignment) => {
            collect_binding_pattern(&assignment.left, bindings);
        }
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                collect_binding_pattern(&property.value, bindings);
            }
            if let Some(rest) = &object.rest {
                collect_binding_pattern(&rest.argument, bindings);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                collect_binding_pattern(element, bindings);
            }
            if let Some(rest) = &array.rest {
                collect_binding_pattern(&rest.argument, bindings);
            }
        }
    }
}

fn resolve_relative_module(from_path: &Path, specifier: &str) -> Result<Option<PathBuf>, String> {
    if !specifier.starts_with('.') {
        return Ok(None);
    }

    let from_dir = from_path.parent().ok_or_else(|| {
        format!(
            "module path has no parent directory: {}",
            from_path.display()
        )
    })?;
    let candidate = from_dir.join(specifier);
    let candidates = [
        candidate.clone(),
        append_extension(&candidate, "js"),
        append_extension(&candidate, "mjs"),
        append_extension(&candidate, "cjs"),
        append_extension(&candidate, "jsx"),
        append_extension(&candidate, "ts"),
        append_extension(&candidate, "tsx"),
        candidate.join("index.js"),
        candidate.join("index.mjs"),
        candidate.join("index.cjs"),
        candidate.join("index.jsx"),
        candidate.join("index.ts"),
        candidate.join("index.tsx"),
    ];

    candidates
        .iter()
        .find(|path| path.is_file())
        .map(|path| normalize_existing_path(path).map(Some))
        .unwrap_or_else(|| {
            let checked = candidates
                .iter()
                .map(|path| format!("candidate: {}", path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            Err(format!(
                "failed to resolve relative module '{specifier}' from {}; {checked}",
                from_path.display()
            ))
        })
}

fn normalize_existing_path(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve path {}: {error}", path.display()))
}

fn has_top_level_side_effects(program: &Program<'_>) -> bool {
    program.body.iter().any(statement_has_top_level_side_effect)
}

fn statement_has_top_level_side_effect(statement: &Statement<'_>) -> bool {
    match statement {
        Statement::EmptyStatement(_) => false,
        Statement::ImportDeclaration(_) | Statement::ExportAllDeclaration(_) => false,
        Statement::ExportNamedDeclaration(declaration) => declaration
            .declaration
            .as_ref()
            .is_some_and(declaration_has_side_effect),
        Statement::ExportDefaultDeclaration(declaration) => {
            export_default_has_side_effect(&declaration.declaration)
        }
        Statement::FunctionDeclaration(_) => false,
        Statement::ClassDeclaration(class) => class_has_static_block(class),
        Statement::VariableDeclaration(declaration) => {
            declaration.declarations.iter().any(|declaration| {
                declaration
                    .init
                    .as_ref()
                    .is_some_and(expression_has_side_effect)
            })
        }
        Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSEnumDeclaration(_)
        | Statement::TSModuleDeclaration(_)
        | Statement::TSGlobalDeclaration(_)
        | Statement::TSImportEqualsDeclaration(_)
        | Statement::TSExportAssignment(_)
        | Statement::TSNamespaceExportDeclaration(_) => false,
        Statement::ExpressionStatement(_)
        | Statement::DebuggerStatement(_)
        | Statement::DoWhileStatement(_)
        | Statement::ForInStatement(_)
        | Statement::ForOfStatement(_)
        | Statement::ForStatement(_)
        | Statement::IfStatement(_)
        | Statement::LabeledStatement(_)
        | Statement::ReturnStatement(_)
        | Statement::SwitchStatement(_)
        | Statement::ThrowStatement(_)
        | Statement::TryStatement(_)
        | Statement::WhileStatement(_)
        | Statement::WithStatement(_)
        | Statement::BreakStatement(_)
        | Statement::ContinueStatement(_)
        | Statement::BlockStatement(_) => true,
    }
}

fn declaration_has_side_effect(declaration: &Declaration<'_>) -> bool {
    match declaration {
        Declaration::FunctionDeclaration(_) => false,
        Declaration::ClassDeclaration(class) => class_has_static_block(class),
        Declaration::VariableDeclaration(declaration) => {
            declaration.declarations.iter().any(|declaration| {
                declaration
                    .init
                    .as_ref()
                    .is_some_and(expression_has_side_effect)
            })
        }
        Declaration::TSTypeAliasDeclaration(_)
        | Declaration::TSInterfaceDeclaration(_)
        | Declaration::TSEnumDeclaration(_)
        | Declaration::TSModuleDeclaration(_)
        | Declaration::TSGlobalDeclaration(_)
        | Declaration::TSImportEqualsDeclaration(_) => false,
    }
}

fn export_default_has_side_effect(declaration: &ExportDefaultDeclarationKind<'_>) -> bool {
    match declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(_) => false,
        ExportDefaultDeclarationKind::ClassDeclaration(class) => class_has_static_block(class),
        ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => false,
        ExportDefaultDeclarationKind::BooleanLiteral(_)
        | ExportDefaultDeclarationKind::NullLiteral(_)
        | ExportDefaultDeclarationKind::NumericLiteral(_)
        | ExportDefaultDeclarationKind::BigIntLiteral(_)
        | ExportDefaultDeclarationKind::RegExpLiteral(_)
        | ExportDefaultDeclarationKind::StringLiteral(_)
        | ExportDefaultDeclarationKind::Identifier(_)
        | ExportDefaultDeclarationKind::MetaProperty(_)
        | ExportDefaultDeclarationKind::ThisExpression(_)
        | ExportDefaultDeclarationKind::Super(_)
        | ExportDefaultDeclarationKind::FunctionExpression(_)
        | ExportDefaultDeclarationKind::ArrowFunctionExpression(_) => false,
        ExportDefaultDeclarationKind::ClassExpression(class) => class_has_static_block(class),
        ExportDefaultDeclarationKind::TemplateLiteral(_)
        | ExportDefaultDeclarationKind::ArrayExpression(_)
        | ExportDefaultDeclarationKind::AssignmentExpression(_)
        | ExportDefaultDeclarationKind::AwaitExpression(_)
        | ExportDefaultDeclarationKind::BinaryExpression(_)
        | ExportDefaultDeclarationKind::CallExpression(_)
        | ExportDefaultDeclarationKind::ChainExpression(_)
        | ExportDefaultDeclarationKind::ConditionalExpression(_)
        | ExportDefaultDeclarationKind::ImportExpression(_)
        | ExportDefaultDeclarationKind::LogicalExpression(_)
        | ExportDefaultDeclarationKind::NewExpression(_)
        | ExportDefaultDeclarationKind::ObjectExpression(_)
        | ExportDefaultDeclarationKind::ParenthesizedExpression(_)
        | ExportDefaultDeclarationKind::SequenceExpression(_)
        | ExportDefaultDeclarationKind::TaggedTemplateExpression(_)
        | ExportDefaultDeclarationKind::UnaryExpression(_)
        | ExportDefaultDeclarationKind::UpdateExpression(_)
        | ExportDefaultDeclarationKind::YieldExpression(_)
        | ExportDefaultDeclarationKind::PrivateInExpression(_)
        | ExportDefaultDeclarationKind::JSXElement(_)
        | ExportDefaultDeclarationKind::JSXFragment(_)
        | ExportDefaultDeclarationKind::TSAsExpression(_)
        | ExportDefaultDeclarationKind::TSSatisfiesExpression(_)
        | ExportDefaultDeclarationKind::TSTypeAssertion(_)
        | ExportDefaultDeclarationKind::TSNonNullExpression(_)
        | ExportDefaultDeclarationKind::TSInstantiationExpression(_)
        | ExportDefaultDeclarationKind::ComputedMemberExpression(_)
        | ExportDefaultDeclarationKind::StaticMemberExpression(_)
        | ExportDefaultDeclarationKind::PrivateFieldExpression(_)
        | ExportDefaultDeclarationKind::V8IntrinsicExpression(_) => true,
    }
}

fn expression_has_side_effect(expression: &Expression<'_>) -> bool {
    !matches!(
        expression,
        Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::RegExpLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::Identifier(_)
            | Expression::MetaProperty(_)
            | Expression::ThisExpression(_)
            | Expression::Super(_)
            | Expression::FunctionExpression(_)
            | Expression::ArrowFunctionExpression(_)
    )
}

fn class_has_static_block(class: &Class<'_>) -> bool {
    class
        .body
        .body
        .iter()
        .any(|element| matches!(element, ClassElement::StaticBlock(_)))
}

fn span_start(span: Span) -> usize {
    span.start as usize
}

fn span_end(span: Span) -> usize {
    span.end as usize
}
