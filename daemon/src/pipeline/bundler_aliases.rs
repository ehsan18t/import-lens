//! The alias tables a Vite, webpack or Rollup config declares, read statically.
//!
//! A config is a program, and the daemon never runs one: running it would execute arbitrary
//! workspace code to answer a decoration. So the config is parsed, and an alias counts only when
//! both its key and its target are spelled in a form a reader can evaluate without running
//! anything. Everything the scaffolds and the bundlers' own docs write is such a form:
//!
//! * `resolve: { alias: { "@app": … } }` (Vite, webpack) and `resolve: { alias: [{ find, replacement }] }` (Vite);
//! * `alias({ entries: … })` in either shape (`@rollup/plugin-alias`);
//! * a target spelled as a string, `path.resolve(__dirname, "src")` / `path.join(…)` (also bare
//!   `resolve` / `join`, and `process.cwd()`), `import.meta.dirname`, or
//!   `fileURLToPath(new URL("./src", import.meta.url))` (also its `.pathname`).
//!
//! An alias built from a variable or a helper of the config's own is not followed. Missing an alias
//! errs toward a floor, never toward a number: the probe this feeds needs positive evidence, a
//! target file that exists outside `node_modules`.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, CallExpression, Expression, ObjectExpression, ObjectProperty,
    ObjectPropertyKind,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::path::{Path, PathBuf};

/// The config stems whose alias tables are read. Mirrored by the extension's watcher, which must
/// report an edit to any of them (`extension/src/watcherInvalidation.ts`).
pub(crate) const BUNDLER_CONFIG_STEMS: [&str; 3] =
    ["vite.config", "webpack.config", "rollup.config"];
pub(crate) const BUNDLER_CONFIG_EXTENSIONS: [&str; 6] = ["js", "mjs", "cjs", "ts", "mts", "cts"];

/// A config larger than this is not a hand-written alias table; it is skipped unread.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// One alias table per bundler config at or above the document, bounded at the workspace root,
/// nearest first. A config that cannot be read or parsed, or declares no alias, yields no table.
pub(crate) fn alias_tables(
    workspace_root: &Path,
    active_document_path: &Path,
) -> Vec<Vec<(String, PathBuf)>> {
    active_document_path
        .ancestors()
        .skip(1)
        .take_while(|directory| directory.starts_with(workspace_root))
        .flat_map(bundler_configs_in)
        .map(|config| declared_aliases(&config))
        .filter(|aliases| !aliases.is_empty())
        .collect()
}

/// One directory listing rather than a stat per candidate name: eighteen spellings per ancestor
/// would cost more than the resolvers they feed.
fn bundler_configs_in(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut configs = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_bundler_config_name)
        })
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    configs.sort();
    configs
}

fn is_bundler_config_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, extension)| {
        BUNDLER_CONFIG_STEMS.contains(&stem) && BUNDLER_CONFIG_EXTENSIONS.contains(&extension)
    })
}

/// The aliases one config declares, each as (key, absolute target).
pub(crate) fn declared_aliases(config: &Path) -> Vec<(String, PathBuf)> {
    let Some(directory) = config.parent() else {
        return Vec::new();
    };
    if std::fs::metadata(config).map_or(true, |metadata| metadata.len() > MAX_CONFIG_BYTES) {
        return Vec::new();
    }
    let Ok(source) = std::fs::read_to_string(config) else {
        return Vec::new();
    };
    let Ok(source_type) = SourceType::from_path(config) else {
        return Vec::new();
    };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source, source_type).parse();
    if parsed.fatal_error {
        return Vec::new();
    }
    let mut reader = AliasReader {
        directory,
        root_relative_slash: config
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("vite.")),
        aliases: Vec::new(),
    };
    reader.visit_program(&parsed.program);
    reader.aliases
}

struct AliasReader<'p> {
    directory: &'p Path,
    /// Vite reads a target starting with `/` as relative to the project root when that exists.
    root_relative_slash: bool,
    aliases: Vec<(String, PathBuf)>,
}

impl<'a> Visit<'a> for AliasReader<'_> {
    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if !property.computed && property.key.static_name().as_deref() == Some("alias") {
            self.read_table(&property.value);
        }
        walk::walk_object_property(self, property);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if call.callee.get_inner_expression().is_specific_id("alias")
            && let Some(Argument::ObjectExpression(options)) = call.arguments.first()
            && let Some(entries) = property_value(options, "entries")
        {
            self.read_table(entries);
        }
        walk::walk_call_expression(self, call);
    }
}

impl AliasReader<'_> {
    fn read_table(&mut self, table: &Expression<'_>) {
        match table.get_inner_expression() {
            Expression::ObjectExpression(object) => {
                for property in object.properties.iter() {
                    if let ObjectPropertyKind::ObjectProperty(property) = property
                        && !property.computed
                        && let Some(key) = property.key.static_name()
                        && let Some(target) = self.path_value(&property.value)
                    {
                        self.aliases.push((key.into_owned(), target));
                    }
                }
            }
            Expression::ArrayExpression(array) => {
                for element in array.elements.iter() {
                    if let ArrayExpressionElement::ObjectExpression(entry) = element
                        && let Some(Expression::StringLiteral(find)) =
                            property_value(entry, "find").map(Expression::get_inner_expression)
                        && let Some(target) = property_value(entry, "replacement")
                            .and_then(|value| self.path_value(value))
                    {
                        self.aliases.push((find.value.as_str().to_owned(), target));
                    }
                }
            }
            _ => {}
        }
    }

    /// The absolute path an expression spells, or `None` when only running it would tell.
    fn path_value(&self, expression: &Expression<'_>) -> Option<PathBuf> {
        match expression.get_inner_expression() {
            Expression::StringLiteral(literal) => Some(self.literal_path(literal.value.as_str())),
            Expression::TemplateLiteral(template) => template
                .single_quasi()
                .map(|text| self.literal_path(text.as_str())),
            expression if self.is_config_directory(expression) => {
                Some(self.directory.to_path_buf())
            }
            Expression::CallExpression(call) => self.call_path(call),
            Expression::StaticMemberExpression(member) if member.property.name == "pathname" => {
                self.url_path(&member.object)
            }
            _ => None,
        }
    }

    fn call_path(&self, call: &CallExpression<'_>) -> Option<PathBuf> {
        let callee = match call.callee.get_inner_expression() {
            Expression::Identifier(identifier) => identifier.name.as_str(),
            Expression::StaticMemberExpression(member) => member.property.name.as_str(),
            _ => return None,
        };
        match callee {
            "resolve" | "join" => {
                let mut path = self.directory.to_path_buf();
                for argument in call.arguments.iter() {
                    let argument = argument.as_expression()?;
                    if self.is_config_directory(argument.get_inner_expression()) {
                        path = self.directory.to_path_buf();
                        continue;
                    }
                    let Expression::StringLiteral(segment) = argument.get_inner_expression() else {
                        return None;
                    };
                    // `join` appends even a rooted segment; `resolve` restarts at one.
                    let segment = segment.value.as_str();
                    if callee == "join" {
                        path.push(segment.trim_start_matches(['/', '\\']));
                    } else {
                        path.push(segment);
                    }
                }
                Some(normalize(&path))
            }
            "fileURLToPath" => self.url_path(call.arguments.first()?.as_expression()?),
            _ => None,
        }
    }

    /// `new URL("./src", import.meta.url)`: a path relative to the config file.
    fn url_path(&self, expression: &Expression<'_>) -> Option<PathBuf> {
        let Expression::NewExpression(url) = expression.get_inner_expression() else {
            return None;
        };
        if !url.callee.is_specific_id("URL") {
            return None;
        }
        let [relative, base] = url.arguments.as_slice() else {
            return None;
        };
        let base_is_config = matches!(
            base.as_expression().map(Expression::get_inner_expression),
            Some(Expression::StaticMemberExpression(member))
                if member.property.name == "url" && matches!(member.object, Expression::ImportMeta(_))
        );
        let Some(Expression::StringLiteral(relative)) = relative
            .as_expression()
            .map(Expression::get_inner_expression)
        else {
            return None;
        };
        base_is_config.then(|| normalize(&self.directory.join(relative.value.as_str())))
    }

    /// `__dirname`, `import.meta.dirname` and `process.cwd()`, which a config run by its bundler
    /// all read as the config's own directory.
    fn is_config_directory(&self, expression: &Expression<'_>) -> bool {
        match expression {
            Expression::Identifier(identifier) => identifier.name == "__dirname",
            Expression::StaticMemberExpression(member) => {
                member.property.name == "dirname"
                    && matches!(member.object, Expression::ImportMeta(_))
            }
            Expression::CallExpression(call) => {
                call.arguments.is_empty() && call.callee.is_specific_member_access("process", "cwd")
            }
            _ => false,
        }
    }

    fn literal_path(&self, text: &str) -> PathBuf {
        if self.root_relative_slash && text.starts_with('/') {
            let under_root = self.directory.join(text.trim_start_matches('/'));
            if under_root.exists() {
                return normalize(&under_root);
            }
        }
        normalize(&self.directory.join(text))
    }
}

fn property_value<'e, 'a>(
    object: &'e ObjectExpression<'a>,
    name: &str,
) -> Option<&'e Expression<'a>> {
    object
        .properties
        .iter()
        .find_map(|property| match property {
            ObjectPropertyKind::ObjectProperty(property)
                if !property.computed && property.key.static_name().as_deref() == Some(name) =>
            {
                Some(&property.value)
            }
            _ => None,
        })
}

/// Fold `.` and `..` without touching the filesystem: the target may not exist yet, and an alias
/// to a missing file must still resolve once it is created.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("il-bundler-aliases-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("workspace");
            Self { root }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn aliases_of(tag: &str, name: &str, config: &str) -> (Workspace, Vec<(String, PathBuf)>) {
        let workspace = Workspace::new(tag);
        let path = workspace.write(name, config);
        let aliases = declared_aliases(&path);
        (workspace, aliases)
    }

    #[test]
    fn every_documented_vite_spelling_is_read() {
        let (workspace, aliases) = aliases_of(
            "vite",
            "vite.config.ts",
            r#"
            import { defineConfig } from "vite";
            import path from "node:path";
            import { fileURLToPath, URL } from "node:url";
            export default defineConfig({
              resolve: {
                alias: {
                  "@app": fileURLToPath(new URL("./src/app", import.meta.url)),
                  "@lib": path.resolve(__dirname, "src/lib"),
                  "@ui": path.join(import.meta.dirname, "./src", "ui"),
                  "@cwd": path.resolve(process.cwd(), "src/cwd"),
                  "@plain": "./src/plain",
                  [computed]: "./ignored",
                  "@helper": helper("src"),
                },
              },
            });
            "#,
        );
        let root = &workspace.root;
        assert_eq!(
            aliases,
            vec![
                ("@app".to_owned(), root.join("src").join("app")),
                ("@lib".to_owned(), root.join("src").join("lib")),
                ("@ui".to_owned(), root.join("src").join("ui")),
                ("@cwd".to_owned(), root.join("src").join("cwd")),
                ("@plain".to_owned(), root.join("src").join("plain")),
            ]
        );
    }

    #[test]
    fn the_array_and_rollup_plugin_shapes_are_read() {
        let (workspace, aliases) = aliases_of(
            "rollup",
            "rollup.config.mjs",
            r#"
            import alias from "@rollup/plugin-alias";
            export default {
              plugins: [alias({ entries: [
                { find: "utils", replacement: new URL("./src/utils", import.meta.url).pathname },
                { find: /^regex$/, replacement: "./ignored" },
              ] })],
            };
            "#,
        );
        assert_eq!(
            aliases,
            vec![("utils".to_owned(), workspace.root.join("src").join("utils"))]
        );
    }

    #[test]
    fn a_webpack_exact_key_keeps_its_marker() {
        let (workspace, aliases) = aliases_of(
            "webpack",
            "webpack.config.js",
            r#"
            const path = require("path");
            module.exports = { resolve: { alias: { components$: path.resolve(__dirname, "src/components/index.js") } } };
            "#,
        );
        assert_eq!(
            aliases,
            vec![(
                "components$".to_owned(),
                workspace
                    .root
                    .join("src")
                    .join("components")
                    .join("index.js")
            )]
        );
    }

    #[test]
    fn configs_are_found_at_and_above_the_document_but_never_above_the_workspace() {
        let workspace = Workspace::new("discovery");
        let project = workspace.root.join("project");
        workspace.write(
            "vite.config.js",
            r#"export default { resolve: { alias: { outside: "./x" } } };"#,
        );
        workspace.write(
            "project/vite.config.mjs",
            r#"export default { resolve: { alias: { nearest: "./x" } } };"#,
        );
        workspace.write(
            "project/notvite.config.js",
            r#"export default { resolve: { alias: { other: "./x" } } };"#,
        );
        let document = workspace.write("project/src/page.js", "");

        let tables = alias_tables(&project, &document);
        assert_eq!(
            tables,
            vec![vec![("nearest".to_owned(), project.join("x"))]]
        );
    }
}
