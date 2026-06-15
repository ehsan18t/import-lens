use crate::{
    cache::key::{FileFingerprint, fingerprints_are_current, fingerprints_for_paths},
    ipc::protocol::ImportRuntime,
    pipeline::resolver::{create_resolver, normalize_existing_path, resolve_module_path},
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{BindingPattern, Declaration, ExportDefaultDeclarationKind, Program, Statement};
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_resolver::Resolver;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};
use oxc_syntax::module_record::{
    ExportEntry, ExportExportName, ExportImportName, ExportLocalName, ImportEntry,
    ImportImportName, ModuleRecord as OxcModuleRecord,
};
use oxc_transformer::{TransformOptions, Transformer};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

static GRAPH_CACHE: OnceLock<papaya::HashMap<(PathBuf, ImportRuntime), CachedModuleGraph>> =
    OnceLock::new();

pub const MAX_GRAPH_MODULES: usize = 2_000;
pub const MAX_MODULE_SOURCE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_GRAPH_SOURCE_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone)]
struct CachedModuleGraph {
    graph: Arc<ModuleGraph>,
    fingerprints: Vec<FileFingerprint>,
}

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
    pub original_source_bytes: usize,
    pub imports: Vec<ImportEdge>,
    pub external_imports: Vec<ExternalImportEdge>,
    pub import_statement_spans: Vec<(usize, usize)>,
    pub export_specifier_statement_spans: Vec<(usize, usize)>,
    pub exports: Vec<ExportRecord>,
    pub reexports: Vec<ReExportRecord>,
    pub star_exports: Vec<StarExportRecord>,
    pub local_bindings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GraphDiagnostic {
    pub stage: String,
    pub message: String,
    pub details: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ImportEdge {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_names: Vec<String>,
    pub imported_bindings: Vec<ImportedBinding>,
}

#[derive(Debug, Clone)]
pub struct ExternalImportEdge {
    pub specifier: String,
    pub imported_name: String,
    pub local_name: String,
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
    pub diagnostics: Vec<GraphDiagnostic>,
    pub dependency_paths: Vec<PathBuf>,
    path_to_id: HashMap<PathBuf, ModuleId>,
}

impl Default for ModuleGraph {
    fn default() -> Self {
        Self {
            entry_id: ModuleId(0),
            modules: Vec::new(),
            diagnostics: Vec::new(),
            dependency_paths: Vec::new(),
            path_to_id: HashMap::new(),
        }
    }
}

impl ModuleGraph {
    pub fn from_parts(
        entry_id: ModuleId,
        modules: Vec<ModuleRecord>,
        diagnostics: Vec<GraphDiagnostic>,
        dependency_paths: Vec<PathBuf>,
    ) -> Self {
        let path_to_id = modules
            .iter()
            .map(|module| (module.path.clone(), module.id))
            .collect();

        Self {
            entry_id,
            modules,
            diagnostics,
            dependency_paths,
            path_to_id,
        }
    }

    pub fn module_by_id(&self, id: ModuleId) -> Option<&ModuleRecord> {
        self.modules.get(id.0)
    }

    pub fn module_id_by_path(&self, path: &Path) -> Option<ModuleId> {
        self.path_to_id.get(path).copied()
    }
}

pub fn build_module_graph(entry_path: &Path) -> Result<ModuleGraph, String> {
    build_module_graph_with_runtime(entry_path, ImportRuntime::Component)
}

pub fn build_module_graph_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<ModuleGraph, String> {
    build_module_graph_with_limits_and_runtime(entry_path, GraphLimits::default(), runtime)
}

pub fn build_module_graph_cached(entry_path: &Path) -> Result<Arc<ModuleGraph>, String> {
    build_module_graph_cached_with_runtime(entry_path, ImportRuntime::Component)
}

pub fn build_module_graph_cached_with_runtime(
    entry_path: &Path,
    runtime: ImportRuntime,
) -> Result<Arc<ModuleGraph>, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    let cache = GRAPH_CACHE.get_or_init(papaya::HashMap::new);
    let pinned = cache.pin();
    let cache_key = (entry_path.clone(), runtime);
    if let Some(graph) = pinned.get(&cache_key) {
        if fingerprints_are_current(&graph.fingerprints) {
            return Ok(Arc::clone(&graph.graph));
        }

        pinned.remove(&cache_key);
    }

    let graph = Arc::new(build_module_graph_with_runtime(&entry_path, runtime)?);
    pinned.insert(
        cache_key,
        CachedModuleGraph {
            fingerprints: module_graph_fingerprints(&entry_path, &graph),
            graph: Arc::clone(&graph),
        },
    );
    Ok(graph)
}

fn module_graph_fingerprints(entry_path: &Path, graph: &ModuleGraph) -> Vec<FileFingerprint> {
    let mut paths = Vec::with_capacity(graph.dependency_paths.len() + 1);
    paths.push(entry_path.to_path_buf());
    paths.extend(graph.dependency_paths.iter().cloned());
    fingerprints_for_paths(paths)
}

pub fn invalidate_module_graph_cache_for_package(package_name: &str) {
    let Some(cache) = GRAPH_CACHE.get() else {
        return;
    };

    let package_segment = format!("node_modules/{package_name}/");
    let pinned = cache.pin();
    let keys = pinned
        .iter()
        .filter(|((path, _runtime), _)| {
            path.to_string_lossy()
                .replace('\\', "/")
                .contains(&package_segment)
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    for key in keys {
        pinned.remove(&key);
    }
}

pub fn clear_module_graph_cache() {
    if let Some(cache) = GRAPH_CACHE.get() {
        cache.pin().clear();
    }
}

pub fn build_module_graph_with_limits(
    entry_path: &Path,
    limits: GraphLimits,
) -> Result<ModuleGraph, String> {
    build_module_graph_with_limits_and_runtime(entry_path, limits, ImportRuntime::Component)
}

pub fn build_module_graph_with_limits_and_runtime(
    entry_path: &Path,
    limits: GraphLimits,
    runtime: ImportRuntime,
) -> Result<ModuleGraph, String> {
    let entry_path = normalize_existing_path(entry_path)?;
    let mut builder = ModuleGraphBuilder::new(limits, runtime);
    let entry_id = builder.load_module(&entry_path)?;
    builder.graph.entry_id = entry_id;
    builder.graph.dependency_paths = builder.dependency_paths.into_iter().collect();
    builder.graph.dependency_paths.sort();

    Ok(builder.graph)
}

struct ModuleGraphBuilder {
    graph: ModuleGraph,
    limits: GraphLimits,
    graph_source_bytes: usize,
    resolver: Resolver,
    dependency_paths: HashSet<PathBuf>,
    circular_edges: HashSet<(PathBuf, PathBuf)>,
    loading_paths: HashSet<PathBuf>,
}

impl ModuleGraphBuilder {
    fn new(limits: GraphLimits, runtime: ImportRuntime) -> Self {
        Self {
            graph: ModuleGraph::default(),
            limits,
            graph_source_bytes: 0,
            resolver: create_resolver(runtime),
            dependency_paths: HashSet::new(),
            circular_edges: HashSet::new(),
            loading_paths: HashSet::new(),
        }
    }
}

impl ModuleGraphBuilder {
    fn load_module(&mut self, path: &Path) -> Result<ModuleId, String> {
        self.load_module_from(path, None)
    }

    fn load_module_from(
        &mut self,
        path: &Path,
        importer: Option<&Path>,
    ) -> Result<ModuleId, String> {
        let path = normalize_existing_path(path)?;
        if let Some(existing) = self.graph.path_to_id.get(&path) {
            if self.loading_paths.contains(&path)
                && let Some(importer) = importer
            {
                let importer = normalize_existing_path(importer)?;
                if self.circular_edges.insert((importer.clone(), path.clone())) {
                    self.graph.diagnostics.push(GraphDiagnostic {
                        stage: "circular_dependency".to_owned(),
                        message: "circular module dependency detected".to_owned(),
                        details: vec![
                            format!("from_path: {}", importer.display()),
                            format!("to_path: {}", path.display()),
                        ],
                    });
                }
            }
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

        let mut resolver_context = ModuleResolverContext {
            resolver: &self.resolver,
            diagnostics: &mut self.graph.diagnostics,
            dependency_paths: &mut self.dependency_paths,
        };
        let prepared_source = prepare_module_source(&path, &source)?;
        let parsed = parse_module(&path, &prepared_source, &mut resolver_context)?;
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
        self.loading_paths.insert(path.clone());
        self.graph_source_bytes = next_graph_source_bytes;
        self.graph.modules.push(ModuleRecord {
            id,
            path: path.clone(),
            source: prepared_source,
            original_source_bytes: source_bytes,
            imports: parsed.imports,
            external_imports: parsed.external_imports,
            import_statement_spans: parsed.import_statement_spans,
            export_specifier_statement_spans: parsed.export_specifier_statement_spans,
            exports: parsed.exports,
            reexports: parsed.reexports,
            star_exports: parsed.star_exports,
            local_bindings: parsed.local_bindings,
        });

        for next_path in next_paths {
            self.load_module_from(&next_path, Some(&path))?;
        }

        self.loading_paths.remove(&path);
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
}

struct ModuleResolverContext<'a> {
    resolver: &'a Resolver,
    diagnostics: &'a mut Vec<GraphDiagnostic>,
    dependency_paths: &'a mut HashSet<PathBuf>,
}

enum ModuleResolution {
    Internal(PathBuf),
    External,
    IgnoredExternal,
}

fn prepare_module_source(path: &Path, source: &str) -> Result<String, String> {
    if path_has_extension(path, "json") {
        return synthetic_json_module(path, source);
    }

    if module_needs_transform(path) {
        return transform_module_source(path, source);
    }

    Ok(source.to_owned())
}

fn synthetic_json_module(path: &Path, source: &str) -> Result<String, String> {
    let json = serde_json::from_str::<Value>(source)
        .map_err(|error| format!("failed to parse JSON module {}: {error}", path.display()))?;
    let literal = serde_json::to_string(&json)
        .map_err(|error| format!("failed to encode JSON module {}: {error}", path.display()))?;
    let mut generated =
        format!("const __importLensJson = {literal};\nexport default __importLensJson;\n");

    if let Some(object) = json.as_object() {
        let mut keys = object.keys().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            if is_safe_js_identifier(key) {
                let quoted_key = serde_json::to_string(key).map_err(|error| {
                    format!("failed to encode JSON key in {}: {error}", path.display())
                })?;
                generated.push_str(&format!(
                    "export const {key} = __importLensJson[{quoted_key}];\n"
                ));
            }
        }
    }

    Ok(generated)
}

fn transform_module_source(path: &Path, source: &str) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "failed to parse module before transform {}; errors: {}",
            path.display(),
            parsed
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&program);
    if !semantic.errors.is_empty() {
        return Err(format!(
            "semantic validation failed before transform {}; errors: {}",
            path.display(),
            semantic
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let transform = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if !transform.errors.is_empty() {
        return Err(format!(
            "failed to transform module {}; errors: {}",
            path.display(),
            transform
                .errors
                .iter()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    Ok(Codegen::new()
        .with_options(CodegenOptions::default())
        .build(&program)
        .code)
}

fn module_needs_transform(path: &Path) -> bool {
    ["ts", "tsx", "mts", "cts", "jsx"]
        .iter()
        .any(|extension| path_has_extension(path, extension))
}

fn path_has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn is_safe_js_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|char| char == '_' || char == '$' || char.is_ascii_alphanumeric())
        && !matches!(
            value,
            "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "null"
                | "new"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "return"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
                | "yield"
                | "await"
        )
}

fn parse_module(
    path: &Path,
    source: &str,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<ParsedModule, String> {
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

    let edges_result = import_edges(path, &parsed.module_record, resolver_context)?;
    Ok(ParsedModule {
        imports: edges_result.imports,
        external_imports: edges_result.external_imports,
        import_statement_spans: edges_result.import_statement_spans,
        export_specifier_statement_spans: export_specifier_statement_spans(&parsed.program),
        exports: export_records(&parsed.module_record),
        reexports: reexport_records(path, &parsed.module_record, resolver_context)?,
        star_exports: star_export_records(path, &parsed.module_record, resolver_context)?,
        local_bindings: local_bindings(&parsed.program),
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
    resolver_context: &mut ModuleResolverContext<'_>,
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
        match resolve_module(path, &specifier, resolver_context)? {
            ModuleResolution::Internal(resolved_path) => {
                push_import_binding(
                    &mut imports,
                    specifier,
                    resolved_path,
                    imported_name,
                    local_name,
                );
            }
            ModuleResolution::External => {
                external_imports.push(ExternalImportEdge {
                    specifier,
                    imported_name,
                    local_name,
                });
            }
            ModuleResolution::IgnoredExternal => {}
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

        match resolve_module(path, specifier.as_str(), resolver_context)? {
            ModuleResolution::Internal(resolved_path) => {
                imports.push(ImportEdge {
                    specifier: specifier.as_str().to_owned(),
                    resolved_path,
                    imported_names: Vec::new(),
                    imported_bindings: Vec::new(),
                });
            }
            ModuleResolution::External => {
                external_imports.push(ExternalImportEdge {
                    specifier: specifier.as_str().to_owned(),
                    imported_name: String::new(),
                    local_name: String::new(),
                });
            }
            ModuleResolution::IgnoredExternal => {}
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
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<Vec<ReExportRecord>, String> {
    module_record
        .indirect_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| reexport_record(path, entry, resolver_context).transpose())
        .collect()
}

fn reexport_record(
    path: &Path,
    entry: &ExportEntry<'_>,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<Option<ReExportRecord>, String> {
    let Some(module_request) = &entry.module_request else {
        return Ok(None);
    };
    let resolved_path = match resolve_module(path, module_request.name.as_str(), resolver_context)?
    {
        ModuleResolution::Internal(resolved_path) => resolved_path,
        ModuleResolution::External | ModuleResolution::IgnoredExternal => return Ok(None),
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
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<Vec<StarExportRecord>, String> {
    module_record
        .star_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| {
            let module_request = entry.module_request.as_ref()?;
            let resolved_path =
                match resolve_module(path, module_request.name.as_str(), resolver_context) {
                    Ok(ModuleResolution::Internal(resolved_path)) => resolved_path,
                    Ok(ModuleResolution::External | ModuleResolution::IgnoredExternal) => {
                        return None;
                    }
                    Err(error) => return Some(Err(error)),
                };
            Some(Ok(StarExportRecord {
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

fn resolve_module(
    from_path: &Path,
    specifier: &str,
    resolver_context: &mut ModuleResolverContext<'_>,
) -> Result<ModuleResolution, String> {
    if specifier_has_query_or_fragment(specifier) {
        push_resolution_diagnostic(
            resolver_context,
            from_path,
            specifier,
            format!("import '{specifier}' contains an unsupported query or fragment"),
        );
        return Ok(ModuleResolution::IgnoredExternal);
    }

    if let Some(kind) = asset_import_kind(specifier) {
        push_asset_diagnostic(resolver_context, from_path, specifier, kind);
        return Ok(ModuleResolution::IgnoredExternal);
    }

    if is_node_builtin_specifier(specifier) {
        push_resolution_diagnostic(
            resolver_context,
            from_path,
            specifier,
            format!("module '{specifier}' is a Node builtin and was kept external"),
        );
        return Ok(ModuleResolution::External);
    }

    let from_dir = from_path.parent().ok_or_else(|| {
        format!(
            "module path has no parent directory: {}",
            from_path.display()
        )
    })?;

    let resolved = match resolve_module_path(resolver_context.resolver, from_dir, specifier) {
        Ok(resolved) => resolved,
        Err(error) => {
            if specifier.starts_with('.') {
                return Err(format!(
                    "failed to resolve relative module '{specifier}' from {}; {error}",
                    from_path.display()
                ));
            }

            push_resolution_diagnostic(
                resolver_context,
                from_path,
                specifier,
                format!("failed to resolve external peer '{specifier}': {error}"),
            );
            return Ok(ModuleResolution::External);
        }
    };

    resolver_context
        .dependency_paths
        .insert(resolved.path.clone());
    Ok(ModuleResolution::Internal(resolved.path))
}

fn specifier_has_query_or_fragment(specifier: &str) -> bool {
    specifier.contains('?') || specifier.contains('#')
}

fn asset_import_kind(specifier: &str) -> Option<&'static str> {
    let extension = Path::new(specifier)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())?
        .to_ascii_lowercase();

    if is_javascript_module_extension(&extension) {
        return None;
    }

    match extension.as_str() {
        "css" | "scss" | "sass" | "less" | "styl" => Some("style"),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" => Some("image"),
        "ttf" | "otf" | "woff" | "woff2" | "eot" => Some("font"),
        _ if specifier.starts_with('.') || specifier.starts_with('/') => Some("asset"),
        _ => None,
    }
}

fn is_javascript_module_extension(extension: &str) -> bool {
    matches!(
        extension,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" | "json"
    )
}

fn push_asset_diagnostic(
    resolver_context: &mut ModuleResolverContext<'_>,
    from_path: &Path,
    specifier: &str,
    kind: &str,
) {
    resolver_context.diagnostics.push(GraphDiagnostic {
        stage: "asset".to_owned(),
        message: format!("non-JavaScript {kind} import kept external: {specifier}"),
        details: vec![
            format!("from_path: {}", from_path.display()),
            format!("specifier: {specifier}"),
            format!("asset_kind: {kind}"),
        ],
    });
}

fn push_resolution_diagnostic(
    resolver_context: &mut ModuleResolverContext<'_>,
    from_path: &Path,
    specifier: &str,
    message: String,
) {
    resolver_context.diagnostics.push(GraphDiagnostic {
        stage: "module_resolution".to_owned(),
        message,
        details: vec![
            format!("from_path: {}", from_path.display()),
            format!("specifier: {specifier}"),
        ],
    });
}

pub fn is_node_builtin_specifier(specifier: &str) -> bool {
    let bare = specifier.strip_prefix("node:").unwrap_or(specifier);
    matches!(
        bare,
        "assert"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "domain"
            | "events"
            | "fs"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "repl"
            | "stream"
            | "string_decoder"
            | "timers"
            | "tls"
            | "tty"
            | "url"
            | "util"
            | "v8"
            | "vm"
            | "worker_threads"
            | "zlib"
    )
}

fn span_start(span: Span) -> usize {
    span.start as usize
}

fn span_end(span: Span) -> usize {
    span.end as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_TEMP_GRAPH_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_graph_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let id = NEXT_TEMP_GRAPH_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
        let process_id = std::process::id();
        let path =
            std::env::temp_dir().join(format!("import-lens-graph-{process_id}-{suffix}-{id}"));
        fs::create_dir_all(&path).expect("temp graph workspace should be created");
        path
    }

    #[test]
    fn graph_resolves_and_transforms_mts_and_cts_modules() {
        let workspace = temp_graph_workspace();

        for extension in ["mts", "cts"] {
            let entry = workspace.join(format!("entry.{extension}"));
            let dep = workspace.join(format!("dep.{extension}"));
            fs::write(
                &entry,
                "import { value } from './dep';\nexport const answer: number = value;\n",
            )
            .expect("entry module should be written");
            fs::write(&dep, "export const value: number = 42;\n")
                .expect("dep module should be written");

            let graph = build_module_graph(&entry).expect("graph should build");

            assert_eq!(graph.modules.len(), 2);
            assert!(
                graph
                    .modules
                    .iter()
                    .all(|module| !module.source.contains(": number")),
                "{graph:?}",
            );
        }

        fs::remove_dir_all(workspace).expect("temp graph workspace should be removed");
    }
}
