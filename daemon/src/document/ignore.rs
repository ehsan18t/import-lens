use crate::ipc::protocol::DetectedImport;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLensIgnoreRule {
    pub kind: ImportLensIgnoreRuleKind,
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportLensIgnoreRuleKind {
    Package,
    Import,
    Path,
}

pub fn parse_import_lens_ignore(contents: &str) -> Vec<ImportLensIgnoreRule> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(parse_rule_line)
        .collect()
}

pub fn should_ignore_import(
    detected: &DetectedImport,
    source_file: &str,
    rules: &[ImportLensIgnoreRule],
) -> bool {
    rules.iter().any(|rule| match rule.kind {
        ImportLensIgnoreRuleKind::Package => {
            glob_matches_exact(&rule.pattern, &detected.package_name)
        }
        ImportLensIgnoreRuleKind::Import => glob_matches_exact(&rule.pattern, &detected.specifier),
        ImportLensIgnoreRuleKind::Path => glob_matches_path(&rule.pattern, source_file),
    })
}

pub fn load_import_lens_ignore(start_file_path: &Path) -> Vec<ImportLensIgnoreRule> {
    let Some(ignore_path) = find_import_lens_ignore(start_file_path) else {
        return Vec::new();
    };

    fs::read_to_string(ignore_path)
        .map(|contents| parse_import_lens_ignore(&contents))
        .unwrap_or_default()
}

/// Resolves the effective `.importlensignore` rules for a document, memoizing by
/// the document's directory.
///
/// The nearest-ancestor `.importlensignore` walk depends only on the starting
/// directory, so every file in a directory shares one filesystem walk. Callers
/// own the resolver's lifetime and thereby its cache scope: create one resolver
/// per logical batch (for example, one workspace report) so that edits to a
/// `.importlensignore` between batches are always re-read, while a single-file
/// operation can use a throwaway resolver.
#[derive(Default)]
pub struct IgnoreRuleResolver {
    by_directory: Mutex<HashMap<PathBuf, Arc<Vec<ImportLensIgnoreRule>>>>,
}

impl IgnoreRuleResolver {
    pub fn rules_for(&self, active_document_path: &Path) -> Arc<Vec<ImportLensIgnoreRule>> {
        let directory = active_document_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();

        if let Ok(mut cache) = self.by_directory.lock() {
            if let Some(rules) = cache.get(&directory) {
                return Arc::clone(rules);
            }
            let rules = Arc::new(load_import_lens_ignore(active_document_path));
            cache.insert(directory, Arc::clone(&rules));
            return rules;
        }

        Arc::new(load_import_lens_ignore(active_document_path))
    }
}

fn find_import_lens_ignore(start_file_path: &Path) -> Option<std::path::PathBuf> {
    let mut current = start_file_path.parent()?.to_path_buf();

    loop {
        let candidate = current.join(".importlensignore");

        if candidate.is_file() {
            return Some(candidate);
        }

        if !current.pop() {
            return None;
        }
    }
}

fn parse_rule_line(line: &str) -> ImportLensIgnoreRule {
    let Some(separator) = line.find(':') else {
        return ImportLensIgnoreRule {
            kind: ImportLensIgnoreRuleKind::Import,
            pattern: line.to_owned(),
        };
    };

    let kind = &line[..separator];
    let pattern = &line[separator + 1..];
    match kind {
        "package" => ImportLensIgnoreRule {
            kind: ImportLensIgnoreRuleKind::Package,
            pattern: pattern.to_owned(),
        },
        "import" => ImportLensIgnoreRule {
            kind: ImportLensIgnoreRuleKind::Import,
            pattern: pattern.to_owned(),
        },
        "path" => ImportLensIgnoreRule {
            kind: ImportLensIgnoreRuleKind::Path,
            pattern: pattern.to_owned(),
        },
        _ => ImportLensIgnoreRule {
            kind: ImportLensIgnoreRuleKind::Import,
            pattern: line.to_owned(),
        },
    }
}

fn glob_matches_path(pattern: &str, file_path: &str) -> bool {
    let normalized_pattern = normalize_path(pattern);
    let normalized_path = normalize_path(file_path);

    if normalized_pattern.starts_with('/') {
        return glob_matches_exact(&normalized_pattern, &normalized_path);
    }

    glob_matches_exact(&normalized_pattern, &normalized_path)
        || suffixes_after_slashes(&normalized_path)
            .into_iter()
            .any(|suffix| glob_matches_exact(&normalized_pattern, suffix))
}

fn suffixes_after_slashes(value: &str) -> Vec<&str> {
    value
        .match_indices('/')
        .map(|(index, _)| &value[index + 1..])
        .collect()
}

fn normalize_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn glob_matches_exact(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut memo = vec![vec![None; value.len() + 1]; pattern.len() + 1];

    glob_matches_at(pattern, value, 0, 0, &mut memo)
}

fn glob_matches_at(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = memo[pattern_index][value_index] {
        return result;
    }

    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == b'*' {
        if pattern.get(pattern_index + 1) == Some(&b'*') {
            glob_matches_at(pattern, value, pattern_index + 2, value_index, memo)
                || (value_index < value.len()
                    && glob_matches_at(pattern, value, pattern_index, value_index + 1, memo))
        } else {
            glob_matches_at(pattern, value, pattern_index + 1, value_index, memo)
                || (value_index < value.len()
                    && value[value_index] != b'/'
                    && glob_matches_at(pattern, value, pattern_index, value_index + 1, memo))
        }
    } else {
        value_index < value.len()
            && pattern[pattern_index] == value[value_index]
            && glob_matches_at(pattern, value, pattern_index + 1, value_index + 1, memo)
    };

    memo[pattern_index][value_index] = Some(result);
    result
}

#[cfg(test)]
mod tests {
    use super::IgnoreRuleResolver;
    use std::fs;
    use std::sync::Arc;

    #[test]
    fn ignore_rule_resolver_memoizes_by_directory() {
        let dir = std::env::temp_dir().join(format!(
            "import-lens-ignore-resolver-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir should be created");
        fs::write(dir.join(".importlensignore"), "package:react\n").expect("ignore file");

        let resolver = IgnoreRuleResolver::default();
        let first = resolver.rules_for(&dir.join("a.ts"));
        let same_dir = resolver.rules_for(&dir.join("b.ts"));

        assert_eq!(first.len(), 1);
        assert!(
            Arc::ptr_eq(&first, &same_dir),
            "same-directory lookups should return the cached rules"
        );

        // The resolver memoizes for its lifetime: an edit is not re-read until a
        // fresh resolver is created, which is what per-report resolvers rely on.
        fs::write(
            dir.join(".importlensignore"),
            "package:vue\npackage:svelte\n",
        )
        .expect("rewrite");
        let after_edit = resolver.rules_for(&dir.join("c.ts"));
        assert!(Arc::ptr_eq(&first, &after_edit));

        let fresh = IgnoreRuleResolver::default().rules_for(&dir.join("a.ts"));
        assert_eq!(fresh.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }
}
