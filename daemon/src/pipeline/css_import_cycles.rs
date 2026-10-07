//! Which `@import` edges close a cycle, so a stylesheet tree can be bundled the way a browser
//! applies it.
//!
//! A browser walks `@import`s depth-first in source order and skips one whose target is already an
//! ancestor of the importing sheet; every other sheet's rules apply. Lightning CSS has no such rule:
//! its ordering pass lets the closing edge become the cyclic sheet's last importer, and its inlining
//! pass then never enters that sheet from the tree, so every sheet on the cycle vanishes from the
//! bundle. Cutting exactly the edges a browser skips leaves a DAG, which Lightning CSS orders
//! correctly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) type ImportEdge = (PathBuf, PathBuf);

/// The edges a depth-first walk from `entry` finds pointing at a sheet still on its stack.
///
/// `imports` lists one sheet's resolved `@import` targets in the order the walk should take them.
/// Source order makes the cut the one a browser makes; any fixed order still finds an edge on
/// every cycle, so an empty result proves the tree acyclic.
pub(crate) fn closing_edges(
    entry: &Path,
    imports: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> BTreeSet<ImportEdge> {
    let mut walk = Walk {
        imports,
        on_stack: BTreeSet::new(),
        finished: BTreeSet::new(),
        closing: BTreeSet::new(),
    };
    walk.visit(entry);
    walk.closing
}

/// Adjacency in a fixed (sorted) order, for proving a recorded edge set acyclic without the source
/// order that only the cutting pass needs.
pub(crate) fn sorted_adjacency(edges: &BTreeSet<ImportEdge>) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut adjacency = BTreeMap::<PathBuf, Vec<PathBuf>>::new();
    for (from, to) in edges {
        adjacency.entry(from.clone()).or_default().push(to.clone());
    }
    adjacency
}

struct Walk<'a> {
    imports: &'a dyn Fn(&Path) -> Vec<PathBuf>,
    on_stack: BTreeSet<PathBuf>,
    finished: BTreeSet<PathBuf>,
    closing: BTreeSet<ImportEdge>,
}

impl Walk<'_> {
    /// Recursion depth is bounded by the stylesheet tree's file limit, far below any stack limit.
    fn visit(&mut self, sheet: &Path) {
        self.on_stack.insert(sheet.to_path_buf());
        for target in (self.imports)(sheet) {
            if self.on_stack.contains(&target) {
                self.closing.insert((sheet.to_path_buf(), target));
            } else if !self.finished.contains(&target) {
                self.visit(&target);
            }
        }
        self.on_stack.remove(sheet);
        self.finished.insert(sheet.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> impl Fn(&Path) -> Vec<PathBuf> {
        let edges = edges
            .iter()
            .map(|(from, to)| {
                (
                    PathBuf::from(from),
                    to.iter().map(PathBuf::from).collect::<Vec<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        move |sheet| edges.get(sheet).cloned().unwrap_or_default()
    }

    fn edge(from: &str, to: &str) -> ImportEdge {
        (PathBuf::from(from), PathBuf::from(to))
    }

    #[test]
    fn a_dag_has_no_closing_edge_even_where_paths_rejoin() {
        let imports = graph(&[("e", &["a", "b"]), ("a", &["c"]), ("b", &["c"])]);
        assert!(closing_edges(Path::new("e"), &imports).is_empty());
    }

    #[test]
    fn the_cut_is_the_edge_back_to_the_first_sheet_entered() {
        let imports = graph(&[("e", &["a"]), ("a", &["b"]), ("b", &["a"])]);
        assert_eq!(
            closing_edges(Path::new("e"), &imports),
            BTreeSet::from([edge("b", "a")])
        );
    }

    /// Source order decides which sheet is entered first, and so which edge closes the cycle.
    #[test]
    fn source_order_decides_which_edge_is_cut() {
        let imports = graph(&[("e", &["b", "a"]), ("a", &["b"]), ("b", &["a"])]);
        assert_eq!(
            closing_edges(Path::new("e"), &imports),
            BTreeSet::from([edge("a", "b")])
        );
    }

    #[test]
    fn a_self_import_is_cut() {
        let imports = graph(&[("e", &["e"])]);
        assert_eq!(
            closing_edges(Path::new("e"), &imports),
            BTreeSet::from([edge("e", "e")])
        );
    }
}
