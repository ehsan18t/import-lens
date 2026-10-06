# Compiler-Stack Upgrade — Sources, API, and Our Surface

## Where the changelogs live

### rolldown (exact-pinned candidate bundler crate)
- **Releases:** https://github.com/rolldown/rolldown/releases — tagged **`vX.Y.Z`**.
  Read every release in range: the Rust crate API has no semver contract, so
  breaking crate changes appear under any heading and any version number.
- **Versions:** `https://crates.io/api/v1/crates/rolldown`.
- **Rust-crate policy (no semver, no docs guarantee):** https://rolldown.rs/apis/rust-crates
- The `oxc_resolver` range rides in the `rolldown_resolver` workspace crate
  (`https://crates.io/api/v1/crates/rolldown_resolver/<ver>/dependencies`), not in
  `rolldown` itself.

### OXC monorepo (parser/ast/semantic/transformer/minifier/codegen/…)
- **Releases:** https://github.com/oxc-project/oxc/releases
  - Crate releases are tagged **`crates_vX.Y.Z`** (e.g. `crates_v0.138.0`).
  - Non-crate releases are tagged `apps_vX.Y.Z`, `oxlint_vX.Y.Z`, `oxfmt_vX.Y.Z` —
    **ignore these**, they are not our crates. Keep only `^crates_v(\d+\.\d+\.\d+)$`.
- **Which versions exist:** crates.io, not GitHub —
  `https://crates.io/api/v1/crates/<crate>` returns every version with publish dates.
  No auth, no rate limit. Use it to enumerate the range *and* to confirm every
  configured OXC crate published the same version. The updater already uses it (`latestCrateVersion`).
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

**Authenticate from the first call.** Unauthenticated access is ~60 req/hr shared per
IP and in practice returns HTTP 403 on the very first release-list page. `gh` is
installed and logged in, so call these endpoints as `gh api '<path>' --paginate --jq …`
(no `GH_TOKEN` is exported; do not build raw `fetch` calls around one). Filter with
`--jq` or pipe into code; never read raw pages into context.

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
features, open the PR for the real detail:
- `gh pr view <N> --repo oxc-project/oxc` (description) and
  `gh pr diff <N> --repo oxc-project/oxc` (the patch); same for `rolldown/rolldown`.
- The diff also downloads without the API: `https://github.com/oxc-project/oxc/pull/<N>.diff`.

Rolldown release bodies are grouped differently: `### 💥 BREAKING CHANGES`,
`### 🚀 Features`, `### 🐛 Bug Fixes`, `### ⚡ Performance`, `### 🚜 Refactor`, and
often a bare list. Entries carry a scope (`rust:`, `node:`, `binding:`, `cli:`,
`dev:`/`hmr:`, `plugin_*`). `node:`/`binding:`/`cli:`/`hmr:`/`dev:` and
`browser` scopes never reach the Rust crate we embed; keep everything else.

## Our OXC surface (starter map — refresh with `grep -rl 'use oxc_' daemon/src`)

| Crate | Where we use it (verify with grep) | What we call |
|---|---|---|
| `oxc_parser` | `document/{completion,imports}`, `pipeline/minify.rs`, `prefetch.rs` | `Parser::new(...).parse()`, `ParserReturn`, error recovery |
| `oxc_allocator` | `document/{completion,imports}`, `pipeline/minify.rs`, `prefetch.rs` | arena `Allocator`, lifetimes bound to it |
| `oxc_semantic` | `pipeline/minify.rs` | linked-chunk validation before minification |
| `oxc_minifier` | `pipeline/minify.rs` | `Minifier`, `MinifierOptions`, mangling metadata |
| `oxc_codegen` | `pipeline/minify.rs` | `Codegen::new().with_options(CodegenOptions::minify()).with_scoping(..).with_private_member_mappings(..).build()` |
| `oxc_span` | `document/{completion,imports,script_regions}`, `pipeline/minify.rs` | `Span`, source ranges, `SourceType` |
| `oxc_syntax` | `document/{completion,imports}` | syntax metadata |
| `oxc_resolver` | `pipeline/resolver.rs` | `Resolver`, `ResolveOptions`, root resolution from active doc path |
| `fast-glob` | `pipeline/resolver.rs` | `glob_match` for `sideEffects`, behind a copy of `rolldown_common::side_effects::glob_match_with_normalized_pattern` |
| `lightningcss` (standalone pin) | `pipeline/{assets,css_dependencies}.rs` | stylesheet bundling and minification for asset bytes |
| `rolldown` (+`rolldown_common`, `rolldown_error`) | `engine/{adapter,plugin}.rs`, and `ModuleId` in `pipeline/resolver.rs` | `BundlerBuilder`/`BundlerOptions`/`generate()`, plugin hooks (`resolve_id`/`load`/`module_parsed`), `OutputChunk.exports`, `RenderedModule::rendered_length()`, `treeshake`, `preserve_entry_signatures`, `code_splitting`, `resolve.{condition_names,main_fields}` |

`oxc_ast`, `oxc_ast_visit`, and `oxc_transformer` are no longer direct
dependencies — they reach us only transitively through rolldown's build.

The minifier and codegen APIs break most often and flow straight into
`pipeline/minify.rs`; the rolldown plugin/output APIs flow into
`engine/{adapter,plugin}.rs` — check those hardest.

## Known gotchas

- **AST nodes are `#[non_exhaustive]`** (a real past BREAKING). `match` on OXC AST
  enums needs a wildcard arm; adding one is often the migration for "non-exhaustive"
  breakages. Note `document/imports.rs` still matches
  `oxc_syntax::module_record::{ImportImportName, ExportImportName}` with no wildcard —
  a new variant there breaks the build.
- **`AstBuilder` method signatures change** between minors — expect codegen/transform
  construction sites to need edits. (We currently construct no AST nodes.)
- **`debug_assert!` guards in OXC hide release-mode misbehavior.** Historical
  example (from when we ran `oxc_transformer` directly): `Transformer`
  `debug_assert!`ed that its `Scoping` came from
  `SemanticBuilder::new().with_enum_eval(true)`; a debug build panicked loudly
  while the **shipped release daemon silently emitted worse code** —
  `Level["High"] = 1 + Level["Low"]` instead of `Level["High"] = 1`. Bigger
  bundle, no error. The transformer now runs inside rolldown, but the lesson
  generalizes: when an oxc guard is a `debug_assert!`, a test that only asserts
  "it did not panic" proves nothing about the binary users run. Assert on the
  emitted output instead.
- **Minifier/codegen output shifts** across versions, silently changing our size
  numbers. `pnpm test:accuracy` alone is NOT a gate (coarse ~75% esbuild tolerance,
  no stored baseline) — the real check is the before/after byte-count baseline plus
  the `minify_source` probe in SKILL.md steps 6–7. And the baseline only sees what
  the fixtures express: confirm coverage before trusting an unchanged diff.
- **Struct literals over OXC options are fragile.** A new public field (e.g.
  `MangleOptions::reserved` in 0.139.0) breaks any construction that names every
  field. Prefer `..Default::default()`.
- **Coordination + apply are updater-enforced** (details in SKILL.md step 6 +
  guardrails): all monorepo crates share ONE version, `oxc_resolver` and `rolldown`
  are independent but must resolve as one graph, `oxc_mangler` is banned as a
  *direct* dep only, and `pnpm deps:update:compiler` edits the coordinated files and
  regenerates the graph fingerprint — don't hand-edit pins. Invoke it as
  `pnpm deps:update:compiler --rolldown <ver> [--oxc <ver>] [--resolver <ver>]`;
  a bare `--` is tolerated but unnecessary under pnpm.
