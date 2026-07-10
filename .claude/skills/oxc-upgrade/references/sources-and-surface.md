# OXC Upgrade — Sources, API, and Our Surface

## Where the changelogs live

### OXC monorepo (parser/ast/semantic/transformer/minifier/codegen/…)
- **Releases:** https://github.com/oxc-project/oxc/releases
  - Crate releases are tagged **`crates_vX.Y.Z`** (e.g. `crates_v0.138.0`).
  - Non-crate releases are tagged `apps_vX.Y.Z`, `oxlint_vX.Y.Z`, `oxfmt_vX.Y.Z` —
    **ignore these**, they are not our crates. Keep only `^crates_v(\d+\.\d+\.\d+)$`.
- **Which versions exist:** crates.io, not GitHub —
  `https://crates.io/api/v1/crates/<crate>` returns every version with publish dates.
  No auth, no rate limit. Use it to enumerate the range *and* to confirm all ten
  crates published the same version. The updater already uses it (`latestCrateVersion`).
- **Per-crate changelog:** `crates/oxc_<crate>/CHANGELOG.md` — note the directory is
  the full crate name (`crates/oxc_transformer/`, not `crates/transformer/`).
  **Do not rely on it.** It is generated when the release PR opens, so anything merged
  later the same day lands in the tagged source and the aggregate release body but
  never appears in the per-crate file. At `crates_v0.139.0` this hid five
  `oxc_transformer` fixes and one `oxc_semantic` change. The aggregate release body is
  the more complete narrative; the authoritative completeness check is the compare API:
  `GET /repos/oxc-project/oxc/compare/crates_v<old>...crates_v<new>`.

### oxc_resolver (separate repo, separate version)
- **Releases:** https://github.com/oxc-project/oxc-resolver/releases — tagged **`vX.Y.Z`**.
- **Changelog:** https://raw.githubusercontent.com/oxc-project/oxc-resolver/main/CHANGELOG.md
- **Versions:** `https://crates.io/api/v1/crates/oxc_resolver`.

## Enumerating releases in a range via the GitHub REST API

**Authenticate from the first call.** `GH_TOKEN` is already in the environment; send
`Authorization: Bearer $GH_TOKEN`. Unauthenticated access is ~60 req/hr shared per IP
and in practice returns HTTP 403 on the very first release-list page. `gh` is **not
installed** — do not plan around it. Use `node -e 'fetch(...)'` or the context-mode
execute tool.

```
# All monorepo releases (paginate page=1,2,… until empty):
GET https://api.github.com/repos/oxc-project/oxc/releases?per_page=100&page=1
# → array of { tag_name, name, body, html_url, published_at }
# Keep tag_name matching /^crates_v(\d+\.\d+\.\d+)$/ whose version is in (current, target].

# One specific release body:
GET https://api.github.com/repos/oxc-project/oxc/releases/tags/crates_v0.139.0

# Everything that actually changed between two tags (the completeness check):
GET https://api.github.com/repos/oxc-project/oxc/compare/crates_v0.138.0...crates_v0.139.0

# Resolver:
GET https://api.github.com/repos/oxc-project/oxc-resolver/releases?per_page=100
# Keep tag_name /^v(\d+\.\d+\.\d+)$/ in range.
```

Process the JSON in code (filter + sort by semver) and print only the in-range
tags and their categorized bodies — don't read raw pages into context.

## Release-note format (how to categorize)

Each release `body` is grouped by emoji headers. Extract each section:
- `💥 BREAKING CHANGES` — entries look like:
  `<hash> <scope>: [BREAKING] <description> (#<PR>) (<author>)`
- `🚀 Features`
- `⚡ Performance`
- `🐛 Bug Fixes`

Every entry links a PR. For breaking changes on crates we use, and for promising
features, open the PR for the real detail. `gh` is usually NOT on PATH in this
sandbox, so prefer a direct fetch:
- Page (no API-limit cost): `https://github.com/oxc-project/oxc/pull/<N>`
- Diff (no API-limit cost): `https://github.com/oxc-project/oxc/pull/<N>.diff`
- API description: `GET https://api.github.com/repos/oxc-project/oxc/pulls/<N>`
  (body only — for the actual patch use `GET .../pulls/<N>/files` or the `.diff` above).
- `gh pr view <N> --repo oxc-project/oxc` only if `gh` happens to be available.

## Our OXC surface (starter map — refresh with `grep -rl 'use oxc_' daemon/src`)

| Crate | Where we use it (verify with grep) | What we call |
|---|---|---|
| `oxc_parser` | `document/{completion,imports}`, `pipeline/{graph,minify}` | `Parser::new(...).parse()`, `ParserReturn`, error recovery |
| `oxc_allocator` | `document/{completion,imports}`, `pipeline/{graph,minify}` | arena `Allocator`, lifetimes bound to it |
| `oxc_ast` | `pipeline/{graph,minify}` | AST node types |
| `oxc_ast_visit` | `pipeline/graph.rs` | visitor traversal |
| `oxc_semantic` | `pipeline/{graph,minify}` | scope tree, symbols, references |
| `oxc_transformer` | `pipeline/{graph,minify}` | TS/JSX stripping (NOT tree-shaking). **`graph.rs` is the only one that sees real TS/JSX**; `minify.rs` always passes `import-lens-bundle.js` + `SourceType::cjs()`/`mjs()`. Both pass `TransformOptions::default()`, which leaves every `env` pass and legacy decorators OFF. |
| `oxc_minifier` | `pipeline/minify.rs` | `Minifier`, `MinifierOptions`, mangling metadata |
| `oxc_codegen` | `pipeline/{graph,minify}` | `Codegen::new().with_options(CodegenOptions::minify()).with_scoping(..).with_private_member_mappings(..).build()` |
| `oxc_span` | `document/{completion,imports,script_regions}`, `pipeline/{graph,minify}` | `Span`, source ranges |
| `oxc_syntax` | `document/{completion,imports}`, `pipeline/graph.rs` | syntax metadata |
| `oxc_resolver` | `pipeline/{cjs,graph,resolver}` | `Resolver`, `ResolveOptions`, resolve from active doc path |

The AST types (`oxc_ast`), builder, minifier, and codegen APIs are the ones that
break most often, and they flow straight into `pipeline/minify.rs` and
`pipeline/graph.rs` — check those hardest.

## Known gotchas

- **AST nodes are `#[non_exhaustive]`** (a real past BREAKING). `match` on OXC AST
  enums needs a wildcard arm; adding one is often the migration for "non-exhaustive"
  breakages. Note `document/imports.rs` still matches
  `oxc_syntax::module_record::{ImportImportName, ExportImportName}` with no wildcard —
  a new variant there breaks the build.
- **`AstBuilder` method signatures change** between minors — expect codegen/transform
  construction sites to need edits. (We currently construct no AST nodes.)
- **`Transformer` `debug_assert!`s on its `Scoping`.** Lowering an enum needs each
  member's evaluated value, so the scoping must come from
  `SemanticBuilder::new().with_enum_eval(true)`. `new_compiler()` does **not** set it,
  and only `graph.rs` transforms TypeScript, so only it needs the flag.
  The trap is that the guard is a `debug_assert!` and `profile.release` sets no
  `debug-assertions`: a debug build (tests, and the accuracy suite's `cargo run`
  daemon) panics loudly, while the **shipped release daemon silently emits worse code**
  — `Level["High"] = 1 + Level["Low"]` instead of `Level["High"] = 1`. Bigger bundle,
  no error. Generalize the lesson: when an oxc guard is a `debug_assert!`, a test that
  only asserts "it did not panic" proves nothing about the binary users run. Assert on
  the emitted output instead.
- **Minifier/codegen output shifts** across versions, silently changing our size
  numbers. `pnpm test:accuracy` alone is NOT a gate (coarse ~75% esbuild tolerance,
  no stored baseline) — the real check is the before/after byte-count baseline plus
  the `minify_source` probe in SKILL.md steps 6–7. And the baseline only sees what
  the fixtures express: confirm coverage before trusting an unchanged diff.
- **Struct literals over OXC options are fragile.** A new public field (e.g.
  `MangleOptions::reserved` in 0.139.0) breaks any construction that names every
  field. Prefer `..Default::default()`.
- **Coordination + apply are updater-enforced** (details in SKILL.md step 6 +
  guardrails): all monorepo crates share ONE version, `oxc_resolver` is independent,
  `oxc_mangler` is banned as a *direct* dep only, and `pnpm deps:update:oxc` edits the
  coordinated files — don't hand-edit pins. Invoke it as
  `pnpm deps:update:oxc --oxc <ver> --resolver <ver>`; a bare `--` is tolerated but
  unnecessary under pnpm.
