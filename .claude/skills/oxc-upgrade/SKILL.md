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

Five facts about that surface decide most impact calls. Get them wrong and you will
mis-scope the delta in both directions:

- **`graph.rs` transforms real TypeScript and JSX; `minify.rs` never does.**
  `transform_module_source_as` runs `Transformer` over `.ts/.tsx/.mts/.cts/.jsx`
  module sources. `minify.rs` always passes the literal path `import-lens-bundle.js`
  with `SourceType::cjs()`/`mjs()`. So `transformer:` entries are live — via
  `graph.rs` only.
- **`TransformOptions::default()` leaves almost everything off.** It is
  `#[derive(Default)]`, so `EnvOptions` selects no targets and every `env` pass is
  disabled, and `DecoratorOptions::legacy` is `false`. Only the source-type-driven
  TypeScript and JSX passes run. Changes to async-to-generator, class properties or
  legacy decorators cannot reach us until someone enables them.
- **`bundle.rs` strips the `export ` keyword before minification.** So mangler changes
  about *exported* declarations (`export const { a, b } = x`) never reach the normal
  bundle — only the raw/static-entry fallback in `analyze.rs`.
- **The CommonJS IIFE wrapper `;(() => {…})();` exists twice**: `cjs.rs` (minified as
  `SourceType::cjs()`) and the conservative fallback in `file_size.rs` (minified as
  `mjs`). Both take no parameters, so `module`/`exports` are free globals the mangler
  never renames.
- **We call exactly one `Scoping` iterator accessor**: `iter_bindings_in`, in
  `graph.rs`.

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
- Enumerate the versions that exist from **crates.io** (`/api/v1/crates/<crate>`) —
  no auth, no rate limit, and it is what the updater itself uses. Confirm the same
  version exists for all 10 crates while you are there.
- Read the notes from the GitHub release bodies: monorepo tag `crates_vX.Y.Z` at
  `oxc-project/oxc`, resolver tag `vX.Y.Z` at `oxc-project/oxc-resolver`. Send
  `Authorization: Bearer $GH_TOKEN` on the FIRST call — unauthenticated requests hit
  the rate limit almost immediately.
- **Do NOT fall back to the per-crate `crates/<crate>/CHANGELOG.md`.** Those files are
  generated when the release PR opens, so changes merged later the same day are in the
  tagged source and in the aggregate release body but missing from the per-crate
  changelog. (`crates_v0.139.0` lost five `oxc_transformer` fixes and one
  `oxc_semantic` change that way.) The ground truth for "did I see everything" is
  `GET /repos/oxc-project/oxc/compare/crates_v<old>...crates_v<new>`.

### 3. Categorize and FILTER the delta — in code, don't read every entry
A wide range (30+ releases) can be 100+ entries; process them in code so you
don't drown:
- Concatenate all in-range release bodies and split by section.
- **A `💥 BREAKING CHANGES` section is not the breaking set.** `crates_v0.139.0` had
  no such section, yet added a public field to `MangleOptions` (breaking for any
  exhaustive struct literal), widened public return types, and changed mangler and
  transformer output — all filed under `🚀`, `🐛` and `⚡`. For the `minifier`,
  `mangler`, `codegen`, `transformer` and `ecmascript` scopes, read Features, Bug
  Fixes and Performance **in full**. Elsewhere, skim.
- **Filter by scope, in two tiers.**
  - *Direct* — our 10 crates: `allocator, ast, ast_visit, codegen, minifier, parser,
    semantic, span, syntax, transformer`.
  - *Transitive but observable* — not ours, but they reach our output or our types:
    `mangler` (run by `oxc_minifier`, which also re-exports `MangleOptions`),
    `ecmascript` (its side-effect analysis drives minification), `traverse`,
    `data_structures`, and bare `rust` (cross-cutting signature changes).
  - *Ignorable* — `linter`/oxlint, `prettier`/formatter, `language_server`,
    `isolated_declarations`, `napi`, `wasm`, `react_compiler`, and anything scoped
    `examples`.
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
- **First, check that the fixtures can even see this delta — then capture the
  baseline, before touching anything.** For each output-shifting change from step 3,
  ask which accuracy fixture exercises that shape. If none does, the baseline cannot
  detect it, and a green diff proves nothing. Fix that *before* the bump so the
  before-picture is taken on the OLD version, and add a throwaway probe (below) for
  shapes no fixture can reach.
  Then run `IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1 pnpm test:accuracy` — enforcement
  ON, or a registry blip yields a baseline that measured nothing — and record the
  brotli and minified byte counts it prints per benchmark.
- **Write a throwaway `minify_source` probe.** `daemon/tests/bundle.rs` already imports
  `minify_source`, so a temporary `daemon/tests/oxc_minify_probe.rs` can print minified
  output for one snippet per changed optimization (plus **both** CJS wrapper shapes —
  `cjs.rs`'s with `is_cjs = true`, `file_size.rs`'s with `is_cjs = false`). Run it with
  `--nocapture`, save the output, keep it unchanged through step 7, and delete it before
  the final commit. `.gitignore` does not cover `daemon/tests/*.rs`. Do not commit it:
  a snapshot of a third-party minifier's output is brittle and buys nothing once the
  upgrade lands.
- Bump versions with the existing updater — do NOT hand-edit the pins:
  `pnpm deps:update:oxc --oxc <ver> --resolver <ver>` (omit a flag to take latest; add
  `--dry-run` first to preview, which prints the files it would touch). It considers
  four text files — `daemon/Cargo.toml`, `docs/ImportLens-SRS.md`,
  `scripts/oxc-stack.config.mjs`, and `package.json` — and writes only those that
  actually change. `package.json` carries no oxc version; the updater merely re-asserts
  its `deps:update:*` scripts, so normally it comes out byte-identical and is skipped,
  leaving three. It then runs `pnpm install --lockfile-only` and
  `cargo update -p <crate> --precise` for `oxc_resolver` and each of the ten crates,
  which moves `Cargo.lock`. It touches **no test file** and does **not** rebuild the
  daemon.
- Make the code changes for every breaking item, and adopt the worthwhile features.
- Keep the OXC monorepo crates on ONE coordinated version (the updater enforces this);
  `oxc_resolver` moves independently.

### 7. Verify
- `cargo fmt --check` and `cargo clippy --workspace --all-targets` — must be clean
  (clippy compiles, so it subsumes `cargo build`).
- `pnpm check` (TypeScript) and `pnpm test` — full suite.
- **Re-run the unchanged probe and diff it.** Every differing line must trace to a
  specific PR you identified in step 3. A diff that maps to none of them means stop.
  This, not the accuracy suite, is what tells you exactly *which* optimization moved.
- **Re-run the accuracy baseline and diff (the real risk of an OXC bump).** Run
  `IMPORT_LENS_ACCURACY_REQUIRE_FIXTURES=1 pnpm test:accuracy` again and compare
  against the byte counts from step 6. A skipped fixture is a failure, not a pass.
  Any change is real (fixtures are lockfile-pinned and deterministic) — trace each to
  the specific PR and confirm it is intended. Do this **even if the in-scope breaking
  set was empty**: perf/codegen changes shift bytes without a `[BREAKING]` label.
  The suite detects **only the drift its fixtures can express** — it is necessary, not
  sufficient, which is why step 6 makes you check coverage first.
- Then delete the probe and confirm `git status --short` shows it gone.
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
- `oxc_mangler` is banned as a **direct** dependency of `daemon/Cargo.toml` — that is
  all the ban means, and all `oxc-coordination.test.mjs` enforces. It is already a
  non-optional transitive dependency of `oxc_minifier` and sits in `Cargo.lock`;
  seeing it there is expected. `MangleOptions` and `MangleOptionsKeepNames` are
  re-exported from `oxc_minifier`, so tuning mangling does not reintroduce the crate.
- Don't skip intermediate versions' breaking changes just because the build
  compiles — a compile-clean upgrade can still change runtime behavior.
- Don't accept the upgrade on a red suite, a skipped accuracy fixture, or an
  unexplained probe/baseline diff (see step 7 — the esbuild-tolerance suite alone is
  not a regression detector).
- A green accuracy run with an unchanged baseline is evidence only if you confirmed in
  step 6 that some fixture actually exercises each changed optimization.
