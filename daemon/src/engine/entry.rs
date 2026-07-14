//! Virtual entry generation (spec §6.2). Pure string construction: every
//! requested surface gets a unique positional `__il_entry_<i>_...` alias so
//! strict entry signatures keep it alive, and requested names are emitted as
//! JSON string literals (valid ES2022 module syntax) so arbitrary export
//! names can never break out of the export clause.

use serde_json::to_string as js_string;

pub const VIRTUAL_ENTRY_ID: &str = "import-lens:entry";
pub const TARGET_PREFIX: &str = "import-lens:target/";

pub fn synthetic_target(index: usize) -> String {
    format!("{TARGET_PREFIX}{index}")
}

pub fn virtual_entry_source(entries: &[super::BundleEntry]) -> String {
    let mut source = String::new();
    for (index, entry) in entries.iter().enumerate() {
        let specifier = js_string(&synthetic_target(index)).expect("specifier serializes");
        match &entry.selection {
            super::BundleSelection::Named(names) => {
                for (name_index, name) in names.iter().enumerate() {
                    let exported = js_string(name).expect("export name serializes");
                    source.push_str(&format!(
                        "export {{ {exported} as __il_entry_{index}_export_{name_index} }} from {specifier};\n"
                    ));
                }
            }
            super::BundleSelection::Default => {
                source.push_str(&format!(
                    "export {{ default as __il_entry_{index}_default }} from {specifier};\n"
                ));
            }
            // `export * from` would drop the default export, so namespace and
            // full selections materialize an escaping namespace instead.
            super::BundleSelection::Namespace | super::BundleSelection::Full => {
                source.push_str(&format!(
                    "import * as __il_entry_{index}_namespace from {specifier};\n\
                     export {{ __il_entry_{index}_namespace }};\n"
                ));
            }
        }
    }
    source
}

#[cfg(test)]
mod tests {
    use super::super::{BundleEntry, BundleSelection};
    use super::*;
    use std::path::PathBuf;

    fn entry(selection: BundleSelection) -> BundleEntry {
        BundleEntry {
            entry_path: PathBuf::from("pkg/index.js"),
            package_root: PathBuf::from("pkg"),
            selection,
        }
    }

    #[test]
    fn named_selection_quotes_names_and_aliases_positionally() {
        let source = virtual_entry_source(&[entry(BundleSelection::Named(vec![
            "parse".to_owned(),
            "a-b".to_owned(),
        ]))]);
        assert_eq!(
            source,
            "export { \"parse\" as __il_entry_0_export_0 } from \"import-lens:target/0\";\n\
             export { \"a-b\" as __il_entry_0_export_1 } from \"import-lens:target/0\";\n"
        );
    }

    #[test]
    fn named_selection_escapes_hostile_names() {
        let source =
            virtual_entry_source(&[entry(BundleSelection::Named(vec!["he\"llo".to_owned()]))]);
        assert_eq!(
            source,
            r#"export { "he\"llo" as __il_entry_0_export_0 } from "import-lens:target/0";"#
                .to_owned()
                + "\n"
        );
    }

    #[test]
    fn default_selection_renders_a_default_alias() {
        let source = virtual_entry_source(&[entry(BundleSelection::Default)]);
        assert_eq!(
            source,
            "export { default as __il_entry_0_default } from \"import-lens:target/0\";\n"
        );
    }

    #[test]
    fn namespace_selection_materializes_an_escaping_namespace() {
        let source = virtual_entry_source(&[entry(BundleSelection::Namespace)]);
        assert_eq!(
            source,
            "import * as __il_entry_0_namespace from \"import-lens:target/0\";\n\
             export { __il_entry_0_namespace };\n"
        );
    }

    #[test]
    fn full_selection_uses_the_namespace_form() {
        assert_eq!(
            virtual_entry_source(&[entry(BundleSelection::Full)]),
            virtual_entry_source(&[entry(BundleSelection::Namespace)])
        );
    }

    #[test]
    fn multiple_entries_use_their_own_indexes() {
        let source = virtual_entry_source(&[
            entry(BundleSelection::Named(vec!["x".to_owned()])),
            entry(BundleSelection::Default),
        ]);
        assert_eq!(
            source,
            "export { \"x\" as __il_entry_0_export_0 } from \"import-lens:target/0\";\n\
             export { default as __il_entry_1_default } from \"import-lens:target/1\";\n"
        );
    }

    #[test]
    fn requested_names_matching_alias_shapes_cannot_collide() {
        // Aliases are positional, so a requested name that spells some
        // alias only ever appears as a quoted source name; the emitted
        // alias identifiers stay unique.
        let source = virtual_entry_source(&[entry(BundleSelection::Named(vec![
            "__il_entry_0_export_1".to_owned(),
            "b".to_owned(),
        ]))]);
        assert_eq!(
            source,
            "export { \"__il_entry_0_export_1\" as __il_entry_0_export_0 } from \"import-lens:target/0\";\n\
             export { \"b\" as __il_entry_0_export_1 } from \"import-lens:target/0\";\n"
        );
    }
}
