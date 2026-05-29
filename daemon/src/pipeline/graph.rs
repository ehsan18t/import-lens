use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone)]
pub struct ModuleRecord {
    pub id: ModuleId,
    pub path: PathBuf,
    pub source: String,
    pub imports: Vec<ImportEdge>,
    pub exports: Vec<ExportRecord>,
    pub has_top_level_side_effects: bool,
}

#[derive(Debug, Clone)]
pub struct ImportEdge {
    pub specifier: String,
    pub resolved_path: PathBuf,
    pub imported_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExportRecord {
    pub exported_name: String,
    pub local_name: String,
}

#[derive(Debug, Default, Clone)]
pub struct ModuleGraph {
    pub modules: Vec<ModuleRecord>,
}
