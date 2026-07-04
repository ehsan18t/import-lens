---
name: oxc-upgrade
description: >-
  Upgrade the OXC stack (oxc_parser/ast/semantic/transformer/minifier/codegen/…
  and the separately-versioned oxc_resolver) the RIGHT way — not just bumping
  version numbers until the build passes, but reading every release's changelog
  from the current version up to the target, understanding the breaking changes,
  new features, perf wins and bug fixes, deep-diving the PRs when needed, mapping
  them against how THIS codebase uses OXC, and then making the necessary code
  changes. Use this whenever asked to update/upgrade/bump OXC, oxc_parser,
  oxc_resolver, or "the oxc stack", to move to a new oxc version, or to check
  what a newer OXC offers us. Do NOT just run the version updater and stop.
---

# OXC Upgrade

## Why this is not a version bump

The trap: bump the pins until `cargo build` passes. The build can pass while
behavior **silently changes** — especially minifier/codegen output, which moves our
size numbers — and you miss new features that could delete or speed up our own code.
So the job is: **understand the full delta current→target, decide what it means for
our usage, then apply both the bump and the code changes it implies.**

## What we depend on (the surface to check against)

Two independently-versioned lines:

- **OXC monorepo crates** (all pinned to one coordinated `~x.y.z`):
  `oxc_allocator, oxc_ast, oxc_ast_visit, oxc_codegen, oxc_minifier, oxc_parser,
  oxc_semantic, oxc_span, oxc_syntax, oxc_transformer` — the canonical list lives
  in `scripts/oxc-stack.config.mjs`.
- **`oxc_resolver`** — separate repo, separate version.

Our OXC-using code (grep `use oxc_` under `daemon/src` to refresh this):
`daemon/src/document/{imports,completion,script_regions}.rs` and
`daemon/src/pipeline/{cjs,graph,minify,resolver}.rs`.

Current versions: read `currentOxcVersion` and `currentResolverVersion` from
`scripts/oxc-stack.config.mjs`.

See `references/sources-and-surface.md` for the exact changelog URLs, the GitHub
API calls, the release-note categorization, and per-crate/per-API gotchas.

## Workflow

### 1. Establish the range
- Read current versions from `scripts/oxc-stack.config.mjs`.
- Determine targets. Default: latest stable for each line. If the user named a
  version, use it. The two lines move independently — resolve each.
- **Semver differs between the lines:** the OXC monorepo is pre-1.0, so a `0.MINOR`
  bump is breaking; `oxc_resolver` uses real semver, so a MAJOR bump (e.g. `11→12`)
  is the breaking signal — read its full CHANGELOG and consider landing it in its
  own commit to isolate blast radius.
- Stop only if **both** lines are already current — each moves on its own schedule.

### 2. Gather EVERY release in the range (not just the endpoints)
Breaking changes accumulate across intermediate versions, so collect notes for
each release in `(current, target]`, for both lines. See
`references/sources-and-surface.md` for the exact API/URLs. In short:
- Monorepo: GitHub releases tagged `crates_vX.Y.Z` at `oxc-project/oxc`.
- Resolver: GitHub releases tagged `vX.Y.Z` at `oxc-project/oxc-resolver`.
- Prefer the GitHub REST API to enumerate releases in range; fall back to the
  per-crate `crates/<crate>/CHANGELOG.md` files at the target tag.

### 3. Categorize and FILTER the delta — in code, don't read every entry
A wide range (30+ releases) can be 100+ entries; process them in code so you
don't drown:
- Concatenate all in-range release bodies. Keep the `💥 BREAKING CHANGES` and
  `🚀 Features` lines (each `[BREAKING]` entry has a scope + PR link); scan
  `⚡ Performance` / `🐛 Bug Fixes` only briefly for items touching our hot paths.
- **Filter to entries whose scope is one of our 10 crates** — `allocator, ast,
  ast_visit, codegen, minifier, parser, semantic, span, syntax, transformer`. A
  `[BREAKING]` whose scope is NOT one of these is out of scope — skip it. Commonly
  ignorable scopes in `crates_v*` notes: `linter`/oxlint, `prettier`/formatter,
  `language_server`, `isolated_declarations`, `napi`, `wasm`.
- Deep-dive (step 4) only the filtered handful — expect ~100 entries to reduce to a few.

### 4. Deep-dive where the note isn't enough
For every breaking change touching a crate we use, and for any feature that looks
like it could replace/simplify our code, open the linked PR
(`github.com/oxc-project/oxc/pull/<N>`) and read the actual API change,
migration notes, and examples. A one-line changelog entry is rarely enough to
make a correct code change.

### 5. Map the delta onto our code
For each relevant change, grep our surface for the affected API and decide impact:
- **Breaking** → we MUST adapt. Find every call site and note the migration.
- **Feature** → could it replace hand-rolled logic in `pipeline/` or `document/`,
  or make analysis faster/more accurate? Note it as an opportunity.
- **Perf/bugfix** → does it touch parsing/minification/codegen/resolution in a way
  that changes our size numbers or fixes a case we work around? Note it.

Produce a short impact table: change → PR → our file(s) → required/optional action.

### 6. Apply
- **First, capture the accuracy baseline — before touching anything.** Run
  `pnpm test:accuracy` and record both the brotli and minified ImportLens byte counts
  it prints per benchmark. Fixtures are deterministic, so this is your before-picture
  (the suite's own pass/fail is only a coarse ~75% esbuild check with no baseline —
  the diff you take yourself in step 7 is what catches real drift).
- Bump versions with the existing updater (it edits `daemon/Cargo.toml`,
  `scripts/oxc-stack.config.mjs`, `package.json`, the lockfiles, the SRS, the
  dependency-policy test, and the VSIX-manifest test together, and validates the
  pins): `pnpm deps:update:oxc -- --oxc <ver> --resolver <ver>` (the `--` stops pnpm
  from swallowing the flags; omit a flag to take latest; add `--dry-run` first to
  preview). Do NOT hand-edit the pins — the updater
  keeps everything coordinated (including `oxc-stack.config.mjs`, the authoritative
  current-version source in step 1).
- Make the code changes for every breaking item, and adopt the worthwhile features.
- Keep the OXC monorepo crates on ONE coordinated version (the updater enforces this);
  `oxc_resolver` moves independently.

### 7. Verify
- `cargo fmt --check` and `cargo clippy --workspace --all-targets` — must be clean
  (clippy compiles, so it subsumes `cargo build`).
- `pnpm check` (TypeScript) and `pnpm test` — full suite.
- **Re-run the accuracy baseline and diff (the real risk of an OXC bump).** Run
  `pnpm test:accuracy` again and compare against the byte counts you captured in
  step 6. Any change is real (deterministic fixtures) — trace each to the specific
  minifier/codegen PR and confirm it is intended before accepting. Do this **even if
  the in-scope breaking set was empty**: perf/codegen changes shift bytes without a
  `[BREAKING]` label, and only this baseline diff catches them.
- **`pnpm package:win32-x64`** — REQUIRED after any daemon change (AGENTS.md). An
  OXC upgrade recompiles the daemon binary; this rebuilds/repackages it for Windows
  and refreshes the daemon binary hash in
  `extension/src/daemon/knownHashes.generated.ts`. Skip it and the extension rejects
  the new binary as a hash mismatch.
- The updater already refreshed the SRS version numbers (§9.4). Hand-edit only the
  prose describing changed APIs/behavior — §9.2/§9.3 and any affected component
  spec — in the same task.

### 8. Report
Summarize: versions moved (both lines), breaking changes and how we adapted each,
features adopted vs deferred (with reasons), perf/accuracy impact observed in the
accuracy suite, and any follow-ups.

## Guardrails
- `oxc_mangler` is banned — never reintroduce it (mangling metadata comes from
  `oxc_minifier`). The updater rejects it.
- Don't skip intermediate versions' breaking changes just because the build
  compiles — a compile-clean upgrade can still change runtime behavior.
- Don't accept the upgrade on a red suite or an unexplained accuracy-baseline diff
  (see step 7 — the esbuild-tolerance suite alone is not a regression detector).
