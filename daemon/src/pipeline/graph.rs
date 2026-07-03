use crate::{
    cache::key::{FileFingerprint, fingerprints_are_current, fingerprints_for_paths},
    ipc::protocol::ImportRuntime,
    pipeline::resolver::{
        ResolverSet, normalize_existing_path, resolve_module_path, shared_resolvers,
    },
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentTargetPropertyIdentifier, BindingPattern, BindingProperty, Declaration,
    ExportDefaultDeclarationKind, Expression, ObjectProperty, Program, Statement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_resolver::Resolver;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
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
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static GRAPH_CACHE: OnceLock<papaya::HashMap<(PathBuf, ImportRuntime), CachedModuleGraph>> =
    OnceLock::new();

pub const MAX_GRAPH_MODULES: usize = 2_000;
pub const MAX_MODULE_SOURCE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_GRAPH_SOURCE_BYTES: usize = 100 * 1024 * 1024;
// Every cached graph retains the full prepared source of all its modules, so
// an unbounded cache can hold gigabytes across a long multi-package session.
pub const MAX_CACHED_GRAPHS: usize = 32;

#[derive(Debug, Clone)]
struct CachedModuleGraph {
    graph: Arc<ModuleGraph>,
    fingerprints: Vec<FileFingerprint>,
    last_used_millis: Arc<AtomicU64>,
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
    pub binding_dependencies: Vec<BindingDependencyRecord>,
    // Root-scope symbol declaration + reference spans, computed once here so the
    // bundle rewriter does not re-parse and re-run semantic analysis per request.
    pub root_symbol_spans: Vec<RootSymbolSpans>,
    pub shorthand_spans: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct RootSymbolSpans {
    pub name: String,
    pub decl: (usize, usize),
    pub references: Vec<(usize, usize)>,
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
pub struct BindingDependencyRecord {
    pub binding_name: String,
    pub referenced_name: String,
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
    full_bundle_minified_len: OnceLock<Option<u64>>,
}

impl Default for ModuleGraph {
    fn default() -> Self {
        Self {
            entry_id: ModuleId(0),
            modules: Vec::new(),
            diagnostics: Vec::new(),
            dependency_paths: Vec::new(),
            path_to_id: HashMap::new(),
            full_bundle_minified_len: OnceLock::new(),
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
            full_bundle_minified_len: OnceLock::new(),
        }
    }

    pub fn module_by_id(&self, id: ModuleId) -> Option<&ModuleRecord> {
        self.modules.get(id.0)
    }

    pub fn module_id_by_path(&self, path: &Path) -> Option<ModuleId> {
        self.path_to_id.get(path).copied()
    }

    pub fn cached_full_bundle_minified_len_or_init(
        &self,
        init: impl FnOnce() -> Option<u64>,
    ) -> Option<u64> {
        *self.full_bundle_minified_len.get_or_init(init)
    }

    pub fn cache_full_bundle_minified_len(&self, len: u64) {
        let _ = self.full_bundle_minified_len.set(Some(len));
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
            graph
                .last_used_millis
                .store(crate::time::unix_millis_now(), Ordering::Relaxed);
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
            last_used_millis: Arc::new(AtomicU64::new(crate::time::unix_millis_now())),
        },
    );

    if pinned.len() > MAX_CACHED_GRAPHS {
        let oldest = pinned
            .iter()
            .min_by_key(|(_, cached)| cached.last_used_millis.load(Ordering::Relaxed))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            pinned.remove(&key);
        }
    }

    Ok(graph)
}

pub fn module_graph_cache_len() -> usize {
    GRAPH_CACHE
        .get()
        .map(|cache| cache.pin().len())
        .unwrap_or(0)
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
    resolvers: Arc<ResolverSet>,
    runtime: ImportRuntime,
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
            resolvers: shared_resolvers(),
            runtime,
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
                && self
                    .circular_edges
                    .insert((importer.to_path_buf(), path.clone()))
            {
                self.graph.diagnostics.push(GraphDiagnostic {
                    stage: "circular_dependency".to_owned(),
                    message: "circular module dependency detected".to_owned(),
                    details: vec![
                        format!("from_path: {}", importer.display()),
                        format!("to_path: {}", path.display()),
                    ],
                });
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

        // Clone the Arc into a local so the resolver borrow is independent of the
        // mutable self borrows below (diagnostics / dependency_paths).
        let resolvers = Arc::clone(&self.resolvers);
        let mut resolver_context = ModuleResolverContext {
            resolver: resolvers.resolver(self.runtime),
            diagnostics: &mut self.graph.diagnostics,
            dependency_paths: &mut self.dependency_paths,
        };
        let mut prepared_source = prepare_module_source(&path, source)?;
        let parsed = match parse_module(
            &path,
            &prepared_source.source,
            &mut resolver_context,
            prepared_source.validate_semantics,
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                // A JS-like module shipping plain JSX (a package whose .js entry
                // ships untranspiled JSX) fails the mjs parse; retry via the JSX
                // transform so the bundler/minifier see JSX-free source. Anything
                // that still fails (Flow types, genuine syntax errors) returns the
                // original error and falls back safely.
                if module_can_retry_as_jsx(&path)
                    && let Ok(transformed) = transform_module_source_as(
                        &path,
                        &prepared_source.source,
                        SourceType::jsx(),
                    )
                {
                    prepared_source.source = transformed;
                    parse_module(&path, &prepared_source.source, &mut resolver_context, false)?
                } else {
                    return Err(error);
                }
            }
        };
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
            source: prepared_source.source,
            original_source_bytes: source_bytes,
            imports: parsed.imports,
            external_imports: parsed.external_imports,
            import_statement_spans: parsed.import_statement_spans,
            export_specifier_statement_spans: parsed.export_specifier_statement_spans,
            exports: parsed.exports,
            reexports: parsed.reexports,
            star_exports: parsed.star_exports,
            root_symbol_spans: parsed.root_symbol_spans,
            shorthand_spans: parsed.shorthand_spans,
            local_bindings: parsed.local_bindings,
            binding_dependencies: parsed.binding_dependencies,
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
    binding_dependencies: Vec<BindingDependencyRecord>,
    root_symbol_spans: Vec<RootSymbolSpans>,
    shorthand_spans: Vec<(usize, usize)>,
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

fn source_type_for_prepared_module() -> SourceType {
    // The graph and bundler operate on a prepared ESM-like source representation.
    // JSON modules are synthesized as ESM, and TS/JSX inputs are transformed before
    // graph parsing. Keep this as MJS even when the original file was .mts/.cts/.cjs.
    SourceType::mjs()
}

struct PreparedModuleSource {
    source: String,
    // Graph parsing only needs module-record structure for unchanged JS-like files
    // and transformed TS/JSX output. Full compiler syntax validation is deferred to
    // generated bundle/minifier boundaries, where invalid reachable output falls
    // back safely instead of spending a semantic pass on every dependency module.
    validate_semantics: bool,
}

fn prepare_module_source(path: &Path, source: String) -> Result<PreparedModuleSource, String> {
    if path_has_extension(path, "json") {
        return Ok(PreparedModuleSource {
            source: synthetic_json_module(path, &source)?,
            validate_semantics: true,
        });
    }

    if module_needs_transform(path) {
        return Ok(PreparedModuleSource {
            source: transform_module_source(path, &source)?,
            validate_semantics: false,
        });
    }

    Ok(PreparedModuleSource {
        source,
        validate_semantics: false,
    })
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
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    transform_module_source_as(path, source, source_type)
}

fn transform_module_source_as(
    path: &Path,
    source: &str,
    source_type: SourceType,
) -> Result<String, String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse module before transform {}; errors: {}",
            path.display(),
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new().build(&program);
    if semantic.diagnostics.has_errors() {
        return Err(format!(
            "semantic validation failed before transform {}; errors: {}",
            path.display(),
            semantic
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let transform = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if transform.diagnostics.has_errors() {
        return Err(format!(
            "failed to transform module {}; errors: {}",
            path.display(),
            transform
                .diagnostics
                .errors()
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

fn module_can_retry_as_jsx(path: &Path) -> bool {
    !path_has_extension(path, "json") && !module_needs_transform(path)
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
            "arguments"
                | "break"
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
                | "eval"
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
    validate_semantics: bool,
) -> Result<ParsedModule, String> {
    let allocator = Allocator::default();
    let source_type = source_type_for_prepared_module();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || parsed.diagnostics.has_errors() {
        return Err(format!(
            "failed to parse module {}; errors: {}",
            path.display(),
            parsed
                .diagnostics
                .errors()
                .map(|error| format!("{error:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    if validate_semantics {
        let semantic = SemanticBuilder::new_compiler().build(&parsed.program);
        if semantic.diagnostics.has_errors() {
            return Err(format!(
                "semantic validation failed for {}; errors: {}",
                path.display(),
                semantic
                    .diagnostics
                    .errors()
                    .map(|error| format!("{error:?}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }

    let edges_result = import_edges(path, &parsed.module_record, resolver_context)?;
    let analysis = root_scope_analysis(&parsed.program);
    Ok(ParsedModule {
        imports: edges_result.imports,
        external_imports: edges_result.external_imports,
        import_statement_spans: edges_result.import_statement_spans,
        export_specifier_statement_spans: export_specifier_statement_spans(&parsed.program),
        exports: export_records(&parsed.module_record),
        reexports: reexport_records(path, &parsed.module_record, resolver_context)?,
        star_exports: star_export_records(path, &parsed.module_record, resolver_context)?,
        local_bindings: local_bindings(&parsed.program),
        binding_dependencies: analysis.dependencies,
        root_symbol_spans: analysis.symbol_spans,
        shorthand_spans: shorthand_identifier_spans(&parsed.program)
            .into_iter()
            .collect(),
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

#[derive(Debug)]
struct StatementBindingRange {
    start: usize,
    end: usize,
    bindings: Vec<String>,
}

struct RootScopeAnalysis {
    dependencies: Vec<BindingDependencyRecord>,
    symbol_spans: Vec<RootSymbolSpans>,
}

// One root-scope semantic pass yields both the binding-dependency edges and the
// declaration/reference spans the bundle rewriter needs, so the rewriter no
// longer re-parses each module per request. Always builds semantic (not gated on
// binding statements) so import bindings that have no declaration statement still
// get their rename spans recorded.
fn root_scope_analysis(program: &Program<'_>) -> RootScopeAnalysis {
    let semantic = SemanticBuilder::new().with_build_nodes(true).build(program);
    if semantic.diagnostics.has_errors() {
        return RootScopeAnalysis {
            dependencies: Vec::new(),
            symbol_spans: Vec::new(),
        };
    }

    let semantic = semantic.semantic;
    let scoping = semantic.scoping();
    let mut references = Vec::new();
    let mut symbol_spans = Vec::new();
    for symbol_id in scoping.iter_bindings_in(scoping.root_scope_id()) {
        let name = scoping.symbol_name(symbol_id).to_owned();
        let decl_span = scoping.symbol_span(symbol_id);
        let mut symbol_references = Vec::new();
        for reference in semantic.symbol_references(symbol_id) {
            let span = semantic.reference_span(reference);
            references.push((span, name.clone()));
            symbol_references.push((span_start(span), span_end(span)));
        }
        symbol_spans.push(RootSymbolSpans {
            name,
            decl: (span_start(decl_span), span_end(decl_span)),
            references: symbol_references,
        });
    }

    RootScopeAnalysis {
        dependencies: binding_dependencies_from(&statement_binding_ranges(program), &references),
        symbol_spans,
    }
}

fn binding_dependencies_from(
    statement_ranges: &[StatementBindingRange],
    references: &[(Span, String)],
) -> Vec<BindingDependencyRecord> {
    if statement_ranges.is_empty() {
        return Vec::new();
    }

    let mut dependencies = Vec::new();
    for range in statement_ranges {
        for (span, referenced_name) in references {
            let start = span_start(*span);
            let end = span_end(*span);
            if start < range.start || end > range.end {
                continue;
            }

            for binding_name in &range.bindings {
                if binding_name == referenced_name {
                    continue;
                }

                dependencies.push(BindingDependencyRecord {
                    binding_name: binding_name.clone(),
                    referenced_name: referenced_name.clone(),
                });
            }
        }
    }

    dependencies.sort_by(|left, right| {
        left.binding_name
            .cmp(&right.binding_name)
            .then_with(|| left.referenced_name.cmp(&right.referenced_name))
    });
    dependencies.dedup_by(|left, right| {
        left.binding_name == right.binding_name && left.referenced_name == right.referenced_name
    });
    dependencies
}

fn statement_binding_ranges(program: &Program<'_>) -> Vec<StatementBindingRange> {
    program
        .body
        .iter()
        .filter_map(|statement| {
            let mut bindings = Vec::new();
            collect_statement_bindings(statement, &mut bindings);
            bindings.sort();
            bindings.dedup();
            let span = statement.span();
            (!bindings.is_empty()).then_some(StatementBindingRange {
                start: span_start(span),
                end: span_end(span),
                bindings,
            })
        })
        .collect()
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
            | "assert/strict"
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
            | "dns/promises"
            | "domain"
            | "events"
            | "fs"
            | "fs/promises"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "inspector/promises"
            | "module"
            | "net"
            | "os"
            | "path"
            | "path/posix"
            | "path/win32"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "readline/promises"
            | "repl"
            | "stream"
            | "stream/consumers"
            | "stream/promises"
            | "stream/web"
            | "string_decoder"
            | "timers"
            | "timers/promises"
            | "tls"
            | "tty"
            | "url"
            | "util"
            | "util/types"
            | "v8"
            | "vm"
            | "worker_threads"
            | "zlib"
    )
}

pub fn module_provides_export(
    graph: &ModuleGraph,
    module_id: ModuleId,
    exported_name: &str,
    visited: &mut HashSet<(ModuleId, String)>,
) -> bool {
    if !visited.insert((module_id, exported_name.to_owned())) {
        return false;
    }

    let Some(module) = graph.module_by_id(module_id) else {
        return false;
    };

    if module
        .exports
        .iter()
        .any(|export| export.exported_name == exported_name)
    {
        return true;
    }

    for reexport in module
        .reexports
        .iter()
        .filter(|reexport| reexport.exported_name == exported_name)
    {
        if reexport.imported_name == "*" {
            return true;
        }

        if let Some(target_id) = graph.module_id_by_path(&reexport.resolved_path)
            && module_provides_export(graph, target_id, &reexport.imported_name, visited)
        {
            return true;
        }
    }

    for star_export in &module.star_exports {
        if let Some(target_id) = graph.module_id_by_path(&star_export.resolved_path)
            && module_provides_export(graph, target_id, exported_name, visited)
        {
            return true;
        }
    }

    false
}

fn span_start(span: Span) -> usize {
    span.start as usize
}

fn span_end(span: Span) -> usize {
    span.end as usize
}

fn span_bounds(span: Span) -> (usize, usize) {
    (span_start(span), span_end(span))
}

pub(crate) fn shorthand_identifier_spans(program: &Program<'_>) -> HashSet<(usize, usize)> {
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
    }
}
