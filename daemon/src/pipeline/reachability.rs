use crate::pipeline::graph::{ModuleGraph, ModuleId};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Clone)]
pub struct ReachableExports {
    symbols: HashSet<(PathBuf, String)>,
    modules: HashSet<PathBuf>,
    full_modules: HashSet<PathBuf>,
}

impl ReachableExports {
    pub fn contains_module_symbol(&self, path: &Path, exported_name: &str) -> bool {
        self.symbols
            .contains(&(path.to_path_buf(), exported_name.to_owned()))
    }

    pub fn contains_module(&self, path: &Path) -> bool {
        self.modules.contains(path)
    }

    pub fn is_full_module(&self, path: &Path) -> bool {
        self.full_modules.contains(path)
    }

    pub fn merge_from(&mut self, other: &ReachableExports) {
        self.symbols.extend(other.symbols.iter().cloned());
        self.modules.extend(other.modules.iter().cloned());
        self.full_modules.extend(other.full_modules.iter().cloned());
    }

    pub fn mark_module(&mut self, path: PathBuf) {
        self.modules.insert(path);
    }

    pub fn mark_full_module(&mut self, path: PathBuf) {
        self.modules.insert(path.clone());
        self.full_modules.insert(path);
    }

    pub fn mark_module_symbol(&mut self, path: PathBuf, exported_name: String) {
        self.modules.insert(path.clone());
        self.symbols.insert((path, exported_name));
    }
}

pub fn reachable_exports(
    graph: &ModuleGraph,
    requested_exports: &[String],
    include_full_entry: bool,
) -> ReachableExports {
    let mut marker = ReachabilityMarker {
        graph,
        reachable: ReachableExports::default(),
        visiting_symbols: HashSet::new(),
        visiting_modules: HashSet::new(),
        processed_symbols: HashSet::new(),
        processed_full_modules: HashSet::new(),
        processed_side_effect_modules: HashSet::new(),
        processed_side_effect_imports: HashSet::new(),
    };

    if include_full_entry || requested_exports.iter().any(|name| name == "*") {
        marker.mark_module_full(graph.entry_id);
    } else {
        marker.mark_module_reachable(graph.entry_id);
        for requested_export in requested_exports {
            marker.mark_export(graph.entry_id, requested_export);
        }
        marker.include_side_effect_imports(graph.entry_id);
    }

    marker.reachable
}

struct ReachabilityMarker<'a> {
    graph: &'a ModuleGraph,
    reachable: ReachableExports,
    visiting_symbols: HashSet<(ModuleId, String)>,
    visiting_modules: HashSet<ModuleId>,
    processed_symbols: HashSet<(ModuleId, String)>,
    processed_full_modules: HashSet<ModuleId>,
    processed_side_effect_modules: HashSet<ModuleId>,
    processed_side_effect_imports: HashSet<ModuleId>,
}

impl ReachabilityMarker<'_> {
    fn mark_export(&mut self, module_id: ModuleId, exported_name: &str) {
        let key = (module_id, exported_name.to_owned());
        if self.processed_symbols.contains(&key) {
            return;
        }
        if !self.visiting_symbols.insert(key.clone()) {
            return;
        }

        let Some(module) = self.graph.module_by_id(module_id) else {
            return;
        };
        self.mark_module_reachable(module_id);

        if module
            .exports
            .iter()
            .any(|export| export.exported_name == exported_name)
        {
            self.reachable
                .symbols
                .insert((module.path.clone(), exported_name.to_owned()));
        }

        for reexport in module
            .reexports
            .iter()
            .filter(|reexport| reexport.exported_name == exported_name)
        {
            self.reachable
                .symbols
                .insert((module.path.clone(), exported_name.to_owned()));

            if let Some(target_id) = self.graph.module_id_by_path(&reexport.resolved_path) {
                if reexport.imported_name == "*" {
                    self.mark_module_full(target_id);
                } else {
                    self.mark_export(target_id, &reexport.imported_name);
                }
            }
        }

        for star_export in &module.star_exports {
            let Some(target_id) = self.graph.module_id_by_path(&star_export.resolved_path) else {
                continue;
            };
            let Some(target) = self.graph.module_by_id(target_id) else {
                continue;
            };
            if target
                .exports
                .iter()
                .any(|export| export.exported_name == exported_name)
            {
                self.reachable
                    .symbols
                    .insert((module.path.clone(), exported_name.to_owned()));
                self.mark_export(target_id, exported_name);
            }
        }

        self.include_side_effect_imports(module_id);
        self.visiting_symbols.remove(&key);
        self.processed_symbols.insert(key);
    }

    fn mark_module_full(&mut self, module_id: ModuleId) {
        if self.processed_full_modules.contains(&module_id) {
            return;
        }
        if !self.visiting_modules.insert(module_id) {
            return;
        }

        let Some(module) = self.graph.module_by_id(module_id) else {
            return;
        };
        self.mark_module_reachable(module_id);
        self.reachable.full_modules.insert(module.path.clone());

        for export in &module.exports {
            self.reachable
                .symbols
                .insert((module.path.clone(), export.exported_name.clone()));
        }

        for reexport in &module.reexports {
            self.reachable
                .symbols
                .insert((module.path.clone(), reexport.exported_name.clone()));
            if let Some(target_id) = self.graph.module_id_by_path(&reexport.resolved_path) {
                if reexport.imported_name == "*" {
                    self.mark_module_full(target_id);
                } else {
                    self.mark_export(target_id, &reexport.imported_name);
                }
            }
        }

        for star_export in &module.star_exports {
            if let Some(target_id) = self.graph.module_id_by_path(&star_export.resolved_path) {
                self.mark_module_full(target_id);
            }
        }

        self.include_side_effect_imports(module_id);
        self.visiting_modules.remove(&module_id);
        self.processed_full_modules.insert(module_id);
    }

    fn include_side_effect_imports(&mut self, module_id: ModuleId) {
        if !self.processed_side_effect_imports.insert(module_id) {
            return;
        }

        let Some(module) = self.graph.module_by_id(module_id) else {
            return;
        };

        for import in module
            .imports
            .iter()
            .filter(|import| import.imported_names.is_empty())
        {
            if let Some(target_id) = self.graph.module_id_by_path(&import.resolved_path) {
                self.mark_side_effect_module(target_id);
            }
        }
    }

    fn mark_side_effect_module(&mut self, module_id: ModuleId) {
        if self.processed_side_effect_modules.contains(&module_id) {
            return;
        }
        if !self.visiting_modules.insert(module_id) {
            return;
        }

        self.mark_module_reachable(module_id);
        self.include_side_effect_imports(module_id);
        self.visiting_modules.remove(&module_id);
        self.processed_side_effect_modules.insert(module_id);
    }

    fn mark_module_reachable(&mut self, module_id: ModuleId) {
        if let Some(module) = self.graph.module_by_id(module_id) {
            self.reachable.modules.insert(module.path.clone());
        }
    }
}
