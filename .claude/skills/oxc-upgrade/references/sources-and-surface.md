# OXC Upgrade — Sources, API, and Our Surface

## Where the changelogs live

### OXC monorepo (parser/ast/semantic/transformer/minifier/codegen/…)
- **Releases:** https://github.com/oxc-project/oxc/releases
  - Crate releases are tagged **`crates_vX.Y.Z`** (e.g. `crates_v0.138.0`).
  - Non-crate releases are tagged `apps_vX.Y.Z`, `oxlint_vX.Y.Z`, `oxfmt_vX.Y.Z` —
    **ignore these**, they are not our crates. Keep only `^crates_v(\d+\.\d+\.\d+)$`.
- **Per-crate changelog:** `crates/<crate>/CHANGELOG.md`, e.g.
  https://github.com/oxc-project/oxc/blob/main/crates/parser/CHANGELOG.md
  (raw: `https://raw.githubusercontent.com/oxc-project/oxc/main/crates/<crate>/CHANGELOG.md`).
  The root `CHANGELOG.md` just points here.

### oxc_resolver (separate repo, separate version)
- **Releases:** https://github.com/oxc-project/oxc-resolver/releases — tagged **`vX.Y.Z`**.
- **Changelog:** https://raw.githubusercontent.com/oxc-project/oxc-resolver/main/CHANGELOG.md

## Enumerating releases in a range via the GitHub REST API

Public, no auth needed (unauthenticated limit ~60 req/hr — a few release-list pages
is fine; if a huge delta risks exhausting it, send an `Authorization: Bearer
$GH_TOKEN` header when a token is available, and fetch PR pages/`.diff` for detail
since those don't count against the API limit).
Use `gh api` if available, otherwise `node -e 'fetch(...)'` or the context-mode
fetch/index tool. In this repo's sandbox, `gh` may not be on PATH in bash — try
PowerShell `gh`, or just hit the API directly.

```
# All monorepo releases (paginate page=1,2,… until empty):
GET https://api.github.com/repos/oxc-project/oxc/releases?per_page=100&page=1
# → array of { tag_name, name, body, html_url, published_at }
# Keep tag_name matching /^crates_v(\d+\.\d+\.\d+)$/ whose version is in (current, target].

# One specific release body:
GET https://api.github.com/repos/oxc-project/oxc/releases/tags/crates_v0.139.0

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
| `oxc_transformer` | `pipeline/{graph,minify}` | TS/JSX stripping (NOT tree-shaking) |
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
  breakages.
- **`AstBuilder` method signatures change** between minors — expect codegen/transform
  construction sites to need edits.
- **Minifier/codegen output shifts** across versions, silently changing our size
  numbers. `pnpm test:accuracy` alone is NOT a gate (coarse ~75% esbuild tolerance,
  no stored baseline) — the real check is the before/after byte-count baseline in
  SKILL.md steps 6–7.
- **Coordination + apply are updater-enforced** (details in SKILL.md step 6 +
  guardrails): all monorepo crates share ONE version, `oxc_resolver` is independent,
  `oxc_mangler` is banned, and `pnpm deps:update:oxc` edits every coordinated file —
  don't hand-edit pins.
