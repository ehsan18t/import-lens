use import_lens_daemon::document::{
    analyze_imports, package_json_dependency_entries, package_json_dependency_sections,
    parse_import_lens_ignore, should_ignore_import,
};
use import_lens_daemon::ipc::protocol::{ImportKind, ImportRuntime};

#[test]
fn analyze_imports_handles_static_reexports_dynamic_and_type_only_skips() {
    let source = r#"
import React, { useMemo, type ReactNode } from "react";
import * as z from "zod";
import type { Foo } from "type-only";
export { format as formatDate } from "date-fns/format";
export * from "lodash-es";
const lazy = import("uuid");
const ignored = import(name);
"#;

    let imports = analyze_imports("sample.tsx", source).expect("imports should parse");
    let compact = imports
        .iter()
        .map(|item| {
            (
                item.specifier.as_str(),
                item.package_name.as_str(),
                item.import_kind,
                item.syntax.as_str(),
                item.named.clone(),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        compact,
        vec![
            ("react", "react", ImportKind::Default, "static", vec![]),
            (
                "react",
                "react",
                ImportKind::Named,
                "static",
                vec!["useMemo".to_owned()]
            ),
            ("zod", "zod", ImportKind::Namespace, "static", vec![]),
            (
                "date-fns/format",
                "date-fns",
                ImportKind::Named,
                "reexport",
                vec!["format".to_owned()]
            ),
            (
                "lodash-es",
                "lodash-es",
                ImportKind::Namespace,
                "star_reexport",
                vec![]
            ),
            ("uuid", "uuid", ImportKind::Dynamic, "dynamic", vec![]),
        ]
    );
    assert_eq!(imports[0].runtime, ImportRuntime::Component);
    assert_eq!(imports[0].specifier_range.start.line, 1);
}

#[test]
fn analyze_imports_supports_component_and_astro_regions() {
    let svelte = r#"
<script lang="ts">
  import { writable } from "svelte/store";
</script>
"#;
    let svelte_imports = analyze_imports("Component.svelte", svelte).expect("svelte should parse");
    assert_eq!(svelte_imports.len(), 1);
    assert_eq!(svelte_imports[0].specifier, "svelte/store");
    assert_eq!(svelte_imports[0].runtime, ImportRuntime::Component);
    assert_eq!(svelte_imports[0].specifier_range.start.line, 2);

    let astro = r#"---
import serverOnly from "server-lib";
---
<script>
import clientOnly from "client-lib";
</script>
<script is:inline>
import ignored from "ignored-lib";
</script>
"#;
    let astro_imports = analyze_imports("Page.astro", astro).expect("astro should parse");
    assert_eq!(
        astro_imports
            .iter()
            .map(|item| (item.specifier.as_str(), item.runtime))
            .collect::<Vec<_>>(),
        vec![
            ("server-lib", ImportRuntime::Server),
            ("client-lib", ImportRuntime::Client),
        ]
    );
}

#[test]
fn analyze_imports_handles_empty_astro_frontmatter_without_panicking() {
    let empty_lf = analyze_imports("Page.astro", "---\n---\n<h1>Hi</h1>\n")
        .expect("empty astro frontmatter should parse");
    assert!(empty_lf.is_empty());

    let empty_crlf = analyze_imports("Page.astro", "---\r\n---\r\n<h1>Hi</h1>\r\n")
        .expect("empty CRLF astro frontmatter should parse");
    assert!(empty_crlf.is_empty());
}

#[test]
fn analyze_imports_handles_script_end_tag_with_trailing_whitespace() {
    let svelte = "<script lang=\"ts\">\nimport { writable } from \"svelte/store\";\n</script >\n";
    let imports = analyze_imports("Component.svelte", svelte).expect("svelte should parse");
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].specifier, "svelte/store");
}

#[test]
fn package_json_dependency_ranges_and_sections_are_extracted_in_rust() {
    let source = r#"{
  "dependencies": {
    "react": "^19.0.0",
    "ignored": 1
  },
  "devDependencies": {
    "typescript": "6.0.3"
  }
}"#;

    let entries = package_json_dependency_entries(source);
    assert_eq!(
        entries
            .iter()
            .map(|entry| (
                entry.name.as_str(),
                entry.version.as_str(),
                entry.section.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("react", "^19.0.0", "dependencies"),
            ("typescript", "6.0.3", "devDependencies"),
        ]
    );
    assert_eq!(entries[0].name_range.start.line, 2);
    assert_eq!(entries[0].value_range.start.line, 2);

    let sections = package_json_dependency_sections(source);
    assert_eq!(
        sections
            .iter()
            .map(|section| section.section.as_str())
            .collect::<Vec<_>>(),
        vec!["dependencies", "devDependencies"]
    );
}

#[test]
fn import_lens_ignore_rules_match_package_import_and_path() {
    let source = [
        "# comment",
        "package:moment",
        "import:@internal/*",
        "path:src/generated/**",
    ]
    .join("\n");
    let rules = parse_import_lens_ignore(&source);
    let imports = analyze_imports("src/app.ts", "import value from '@internal/ui';")
        .expect("import should parse");

    assert!(should_ignore_import(
        &imports[0],
        "C:/repo/src/app.ts",
        &rules
    ));

    let react =
        analyze_imports("src/app.ts", "import React from 'react';").expect("react should parse");
    assert!(should_ignore_import(
        &react[0],
        "C:/repo/src/generated/types.ts",
        &rules
    ));
    assert!(!should_ignore_import(
        &react[0],
        "C:/repo/src/app.ts",
        &rules
    ));
}

#[test]
fn jsx_in_plain_js_documents_still_analyzes() {
    let imports = analyze_imports(
        "App.js",
        "import { useState } from 'react';\nexport const App = () => <div />;\n",
    )
    .expect("JSX in .js should analyze");

    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].specifier, "react");
}

#[test]
fn comparison_chains_in_plain_js_still_parse_with_jsx_enabled() {
    let imports = analyze_imports(
        "math.js",
        "import { clamp } from 'lodash';\nexport const inRange = (a, b, c) => a < b && b > c;\n",
    )
    .expect("comparison operators must keep parsing");

    assert_eq!(imports.len(), 1);
}

#[test]
fn builtin_subpath_specifiers_are_not_runtime_packages() {
    for specifier in [
        "fs/promises",
        "dns/promises",
        "stream/promises",
        "stream/web",
        "stream/consumers",
        "timers/promises",
        "readline/promises",
        "path/posix",
        "path/win32",
        "util/types",
        "assert/strict",
        "inspector/promises",
    ] {
        assert!(
            !import_lens_daemon::document::is_runtime_package_specifier(specifier),
            "{specifier} should be treated as a Node builtin"
        );
    }

    assert!(import_lens_daemon::document::is_runtime_package_specifier(
        "fs-extra"
    ));
}

// A TypeScript import whose binding is used only in a type position is erased by the
// compiler, so it costs nothing at runtime. Sending it to the bundler as a runtime
// named import makes the bundler correctly report a missing runtime export, which the
// analyzer turns into a hard zero-size error on code that compiles and runs (spec W4).
//
// The three tests below pin the fix from both sides: it must elide the type-only
// binding, and it must NOT elide anything else.

#[test]
fn type_position_only_named_import_is_elided_and_does_not_return_as_a_namespace() {
    let source = r#"
import { ParseOptions } from "commander";
import { program } from "commander";
const options: ParseOptions = {};
program.parse();
"#;

    let imports = analyze_imports("sample.ts", source).expect("imports should parse");

    assert!(
        !imports
            .iter()
            .any(|item| item.named.iter().any(|name| name == "ParseOptions")),
        "a type-position-only binding must not be sized as a runtime import: {imports:?}"
    );

    // The elided STATEMENT must be gone entirely. If it survives, the
    // `requested_modules` pass re-adds it as a namespace import of the whole package —
    // turning a zero-cost type import into commander's entire weight, which is worse
    // than the bug being fixed. A namespace group carries no `named`, so asserting on
    // `named` alone would pass while that shipped.
    assert_eq!(
        imports
            .iter()
            .filter(|item| item.specifier == "commander")
            .count(),
        1,
        "the elided statement must not reappear as a second (namespace) import: {imports:?}"
    );

    let survivor = imports
        .iter()
        .find(|item| item.specifier == "commander")
        .expect("the value import should survive");
    assert_eq!(survivor.import_kind, ImportKind::Named);
    assert_eq!(survivor.named, vec!["program".to_owned()]);
}

#[test]
fn a_binding_used_as_both_type_and_value_is_not_elided() {
    // A class is a type AND a value. Eliding it would silently under-count.
    let source = r#"
import { Thing } from "pkg-a";
const t: Thing = new Thing();
"#;

    let imports = analyze_imports("sample.ts", source).expect("imports should parse");
    let named: Vec<&str> = imports
        .iter()
        .flat_map(|item| item.named.iter().map(String::as_str))
        .collect();

    assert!(
        named.contains(&"Thing"),
        "a binding referenced as a value must never be elided: {imports:?}"
    );
}

#[test]
fn an_unused_import_is_still_a_runtime_import() {
    // Under verbatimModuleSyntax / isolatedModules TypeScript PRESERVES an unused
    // value import, and it has real runtime cost. Eliding it would under-count.
    let source = r#"
import { unused } from "pkg-b";
export const x = 1;
"#;

    let imports = analyze_imports("sample.ts", source).expect("imports should parse");
    let named: Vec<&str> = imports
        .iter()
        .flat_map(|item| item.named.iter().map(String::as_str))
        .collect();

    assert!(
        named.contains(&"unused"),
        "an unused VALUE import must not be elided: {imports:?}"
    );
}

#[test]
fn a_bare_side_effect_import_still_produces_a_group() {
    // The `requested_modules` pass that would resurrect an elided statement is the
    // same one that legitimately detects `import "pkg"`. Suppressing elided statements
    // must not suppress these.
    let imports = analyze_imports("sample.ts", r#"import "pkg-c";"#).expect("imports should parse");

    assert_eq!(
        imports.len(),
        1,
        "a bare side-effect import must still be detected: {imports:?}"
    );
    assert_eq!(imports[0].specifier, "pkg-c");
}

#[test]
fn javascript_never_elides_imports() {
    // A .js file has no type positions. Eliding there would be a straight under-count,
    // so the semantic pass must not even run.
    let source = r#"
import { thing } from "pkg-d";
export const x = 1;
"#;

    let imports = analyze_imports("sample.js", source).expect("imports should parse");
    let named: Vec<&str> = imports
        .iter()
        .flat_map(|item| item.named.iter().map(String::as_str))
        .collect();

    assert!(
        named.contains(&"thing"),
        "a JavaScript import must never be elided: {imports:?}"
    );
}
