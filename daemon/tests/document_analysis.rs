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
