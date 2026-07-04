# Git Hooks, Lint Gates, Commit Convention, and Dependency-Version Policy — Design

Date: 2026-07-04
Status: awaiting review

This spec covers two related but separately-committed workstreams:

- **Workstream 1 — Quality gates & commit convention** (§ Goal … § D8): git hooks,
  lint/format tooling, conventional-commit enforcement, changelog body-feeding.
- **Workstream 2 — Dependency-version policy & doc alignment** (§ W2): apply the
  tiered version policy (including OXC → patch-only, per the decision below),
  rewrite the pin-enforcement machinery, and fix the stale/obsolete/over-strict
  version rules scattered across instruction and skill docs.

They ship as **separate commits** (see § Commit structure) so each is independently
reviewable; the plan sequences them so nothing is left behind.

## Goal

Add local quality gates for the two languages in this repo — TypeScript (extension,
`scripts/`, CLI) and Rust (daemon) — plus an enforced commit-message convention, so
that AI changelog generation (`scripts/generate-changelog.mjs`) gets reliably
structured input.

Requested gates:

| Hook | Rust | TypeScript |
|---|---|---|
| pre-commit | clippy, cargo-deny, syntax check, auto-format (non-blocking) | syntax check, lint, auto-format (non-blocking) |
| pre-push | build + test | build + test |
| commit-msg | conventional commits, **body (description) mandatory** | same |

## Current state (verified)

- Root `package.json` is the single package (pnpm, `pnpm-workspace.yaml` only holds
  `allowBuilds`). TS is built with tsdown, type-checked with `pnpm check`
  (`tsc --noEmit`), tested with `node --test`. **No linter or formatter exists** —
  only `.editorconfig`.
- Rust workspace (`daemon/`). `rust-toolchain.toml` ships only `rustfmt` — **no
  clippy component**. CI (`validate.yml`) runs `pnpm check`, `cargo fmt --check`,
  `pnpm test`; no clippy, no cargo-deny.
- `clippy.toml` and `deny.toml` were just added as untested samples. Key finding:
  the `clippy.toml` thresholds (`too-many-lines-threshold`,
  `cognitive-complexity-threshold`) configure **allow-by-default lints**
  (`pedantic`/`nursery` groups) — without enabling those lints in
  `[workspace.lints.clippy]`, the thresholds do nothing.
- `cargo-deny` 0.19.0 is installed locally. `deny.toml` has never been run against
  this dependency tree (license allowlist and the `unicode-ident` clarify block are
  unverified).
- Commit history already follows `type(scope): subject` with detailed bodies.
  `cliff.toml` + `generate-changelog.mjs` already parse conventional commits, but
  the changelog script only feeds commit **subjects** (`%s`) to the AI — the bodies
  this design makes mandatory are currently discarded.
- No hook manager installed. Platform: Windows dev machine, Linux CI.

## Decisions

### D1. Hook manager: lefthook

**Chosen: lefthook** (npm devDependency; single static Go binary).

- Declarative `lefthook.yml`: per-hook jobs with glob filters (run Rust jobs only
  when Rust files are staged), parallel execution, `stage_fixed: true` to re-stage
  formatter output — exactly the "format and add, don't block" behavior requested.
- No POSIX-shell dependency, so it is robust on Windows (husky runs hooks through
  Git-for-Windows `sh` and needs hand-written shell scripts plus `lint-staged` for
  staged-file filtering).
- Installed via a root `"prepare": "lefthook install"` script so every
  `pnpm install` wires `.git/hooks` automatically.

Alternatives considered:
- **husky + lint-staged** — two deps, shell-script hooks, weaker Windows story.
- **Plain `core.hooksPath` + hand-rolled `.mjs` scripts** — zero deps and matches
  the repo's script culture, but reimplements staged-file filtering, parallelism,
  skip flags, and re-staging; not worth it.

Escape hatches: `git commit --no-verify` / `LEFTHOOK=0` (documented as
exceptional; CI is the backstop).

### D2. TS lint + format: Biome

**Chosen: Biome** (one devDependency; linter and formatter in one fast binary).

- `biome check --write --staged` in pre-commit fixes lint issues and formats only
  staged files; combined with lefthook `stage_fixed`, fixes are added to the commit
  without blocking it.
- `biome.json` with `formatter.useEditorconfig: true` so the existing
  `.editorconfig` (LF, indent rules) stays the single source of truth for basics.
- Covers `.ts` in `extension/`, `.mjs` in `scripts/` and `cli/`, plus JSON.

Alternatives considered:
- **ESLint + Prettier** — the ecosystem standard, but two tools, config sprawl,
  and much slower; overkill for this repo's size.
- **oxlint** — appealing alignment with the repo's oxc stack, but it has no stable
  formatter yet, so it would still need Prettier/Biome alongside. Revisit once
  oxfmt stabilizes.

One-time cost: an initial `biome check --write .` formatting commit (kept separate
so `git blame` noise is isolated in one commit).

### D3. Commit message enforcement: custom `scripts/check-commit-msg.mjs`

**Chosen: a small hand-rolled validator** run from the `commit-msg` hook, with
tests under the existing `pnpm test:scripts` glob.

Rules enforced:
1. Header matches `type(scope)!?: subject`.
   - `type` ∈ `feat fix perf docs refactor style test chore ci build` — the exact
     set `cliff.toml` already parses (single source: export the list and assert in
     a test that it matches `cliff.toml`).
   - `scope` optional, lowercase `[a-z0-9-]+`.
   - `subject` non-empty, no trailing period, header ≤ 72 chars.
2. **Body mandatory**: blank line after the header, then a body of ≥ 20
   non-whitespace characters (comment lines `#` and diff sections from
   `commit -v` are stripped before checking).
3. Pass-through for machine-generated commits: `Merge …`, `Revert "…"`,
   `fixup!`/`squash!` prefixes.

Alternative considered: **commitlint** (`@commitlint/cli` +
`config-conventional`) — well known and has `body-empty: never`, but it drags in a
large dependency tree for what is ~80 lines of testable code in a repo that
already prefers hand-rolled `.mjs` tooling (`resolve-version`, `check-coverage`,
`generate-changelog`).

A committed `.gitmessage` template (wired via `git config commit.template` in the
`prepare` step) guides the format interactively.

### D4. Rust gate composition

- `rust-toolchain.toml`: add `"clippy"` to `components`.
- **Make the sample `clippy.toml` real** — add to root `Cargo.toml`:

  ```toml
  [workspace.lints.rust]
  # keep default

  [workspace.lints.clippy]
  all = { level = "warn", priority = -1 }   # correctness/suspicious/style/complexity/perf
  too_many_lines = "warn"                    # pedantic — activates too-many-lines-threshold
  cognitive_complexity = "warn"              # nursery — activates cognitive-complexity-threshold
  ```

  and `[lints] workspace = true` in `daemon/Cargo.toml`. (`large_enum_variant` and
  `disallowed_macros` are warn-by-default, so their `clippy.toml` config already
  takes effect once clippy runs.) Deliberately *not* enabling all of
  `pedantic`/`nursery` — that produces noise disproportionate to value.
- **Fix the sample `clippy.toml`**: `todo!`/`unimplemented!` are defined in `core`,
  so the disallowed-macros paths must be `core::todo` / `core::unimplemented`
  (`std::dbg` is correct as-is). Verify empirically with a scratch `dbg!()`/`todo!()`.
- No separate "syntax check" step: `cargo clippy` fully compiles the crate, so it
  subsumes `cargo check`.
- **Tune `deny.toml` by running it**: fix the license allowlist against the real
  dependency tree (expect additions such as `BSD-3-Clause`/`ISC`), delete the
  `unicode-ident` clarify block if the locked version is already licensed
  `Unicode-3.0`, and add any needed `skip` entries for duplicate versions.

### D5. Hook layout (`lefthook.yml`)

```yaml
pre-commit:
  parallel: true
  jobs:
    - name: rust-fmt
      glob: "*.rs"
      run: cargo fmt
      stage_fixed: true            # format + add, never block
    - name: rust-clippy
      glob: ["*.rs", "Cargo.toml", "Cargo.lock", "clippy.toml"]
      run: cargo clippy --workspace --all-targets -- -D warnings
    - name: rust-deny
      glob: ["Cargo.toml", "Cargo.lock", "deny.toml"]   # deps changed → audit
      run: cargo deny check
    - name: ts-biome
      glob: ["*.{ts,mts,mjs,json}"]
      run: pnpm exec biome check --write --staged
      stage_fixed: true
    - name: ts-typecheck
      glob: ["*.{ts,mts}", "tsconfig*.json"]
      run: pnpm check

commit-msg:
  jobs:
    - name: commit-convention
      run: node scripts/check-commit-msg.mjs {1}

pre-push:
  jobs:
    - name: build-and-test
      run: pnpm test                # tsdown build + TS tests + script tests + cargo test --workspace
```

Notes:
- Glob filters mean a TS-only commit never compiles Rust and vice versa.
- `cargo deny check` runs only when dependency manifests change (it audits deps,
  not source), keeping typical commits fast. If `cargo-deny` is not installed the
  job prints an install hint (`cargo install cargo-deny`) and fails — CI enforces
  it regardless (D6).
- `pnpm test` already equals "build + test both languages", satisfying both
  pre-push rows with one command.
- Known tradeoff, documented for contributors: `cargo fmt` + `stage_fixed` on a
  *partially staged* file stages the whole file. Acceptable; rare in practice.

### D6. CI backstop (hooks are bypassable)

Extend `validate.yml`:
- `cargo clippy --workspace --all-targets -- -D warnings` (after the fmt check;
  benefits from the existing Swatinem rust-cache).
- `cargo deny check` via `EmbarkStudios/cargo-deny-action` (pinned).
- `pnpm exec biome ci .`.
- New PR-only job in `ci.yml`: validate all PR commits' messages with the same
  `scripts/check-commit-msg.mjs` (loops over `git log --format=%H` for the PR
  range), so the local hook and CI can never drift.

### D7. Changelog actually consumes the bodies

The entire reason bodies become mandatory is AI changelog quality, but
`generate-changelog.mjs` currently collects `--pretty=format:%s` only. Change it
to collect subject + body per commit (NUL-separated `%s%n%b%x00` records), include
truncated bodies (cap ~600 chars each) in the AI prompt, and extend
`SYSTEM_PROMPT` to use body detail for user-facing phrasing. git-cliff/plain
fallbacks stay subject-based. Update the existing script tests.

### D8. Documentation

- `AGENTS.md` "Git Expectations": codify the commit format, mandatory body, type
  list, and hook behavior.
- `README.md` contributor note: `pnpm install` wires hooks; `--no-verify` is for
  emergencies; how to run each gate manually.

## Dependency version policy

Add every new dependency at its **latest stable release at implementation time**
(resolved then, not guessed now). The version constraint on any dependency — new
or existing — is chosen by the **blast radius of an automatic upgrade**, tightening
only as far as the real risk demands. Goal: stay current automatically wherever
it is safe.

1. **Doesn't break us → track minor + patch (caret `^`).** If no upgrade within
   the current major line can break the project, let it float. Applies to the new
   tooling (lefthook, Biome devDeps; `cargo-deny` CI action pinned to its major
   tag e.g. `@v2`) and most well-behaved libraries.
2. **Minor can break, patch is safe → patch-only (tilde `~`).** Allow patch
   updates only. This is where the **oxc stack** belongs. Semver subtlety: oxc
   core is pre-1.0 (`0.138.0`), so a `0.MINOR` bump is the *breaking* axis —
   "patch only" therefore means `~0.138.0` (accepts `0.138.x`, holds the API), and
   `~11.22.0` for `oxc_resolver`. Bugfix patches flow in automatically; a breaking
   `0.139`/minor jump stays a deliberate, tested step via
   `scripts/update-oxc-stack.mjs`.
3. **Even a patch can break us → pin exact (`=`).** The lockstep case (e.g. a
   Payload-CMS-style ecosystem where patch versions must match across packages).
   Reserved for genuinely that. Biome config's `$schema` pins to the installed
   version for the same reason (schema must match the binary).

# W2. Dependency-version policy & doc alignment (separate workstream)

**Decision (user, 2026-07-04): move OXC to patch-only (`~`).** OXC ceases to be an
exact-pin exception; it moves to tier 2 like the rest. This is *not* just a
manifest edit — the exact-pin is enforced by code, a test, and the updater, and
documented in the SRS. All of that must move together or the tree goes red.

### W2.1 Manifest changes (`daemon/Cargo.toml`)

- All 11 OXC monorepo crates `=0.138.0` → `~0.138.0`.
- `oxc_resolver` `=11.22.0` → `~11.22.0`.
- (Cosmetic, optional: `zstd = "^0.13"` → `~0.13` to match its documented policy;
  functionally identical, so only if it keeps the policy test honest.)

### W2.2 Rewrite the pin-enforcement machinery

These currently *require* exact `=` pins and will fail once the manifest uses `~`:

- **`scripts/oxc-stack-helpers.mjs`** — throws `"Missing exact OXC crate pin"` /
  `"Missing exact oxc_resolver pin"` and checks coordination. Change the contract
  from "exact `=`" to "patch-pin `~`", **keeping the coordination check** (all
  monorepo crates must still share one `~0.MINOR.PATCH`). The resolver keeps its
  own independent `~` pin.
- **`scripts/update-oxc-stack.mjs`** + **`scripts/oxc-stack.config.mjs`** — the
  updater rewrites pinned tokens and the SRS table; it must emit/accept `~` and
  its regex must match `~`-prefixed versions. Its "exact pin" wording in the SRS
  table becomes "patch pin".
- **Tests**: `scripts/test/dependency-policy.test.mjs` (asserts the coordinated
  *exact* pin — retarget to `~`) and `scripts/test/update-oxc-stack.test.mjs`
  (asserts `"exact pin"` table cells and token replacement — retarget to `~` /
  "patch pin"). Keep the coordination and `oxc_mangler`-rejection assertions.

### W2.3 SRS updates (`docs/ImportLens-SRS.md`)

- **§9.3 OXC Versioning Note**: replace the "must use exact requirement syntax
  (`=0.138.0`)" mandate with the patch-pin (`~`) policy, preserving the coordinated-
  stack requirement and the `deps:update:oxc` batch-upgrade flow (now for
  minor/major jumps; patches flow automatically). Note the residual accuracy-drift
  risk is caught by the CI accuracy suite (`ci.yml` runs `run_accuracy: true`), and
  the committed `Cargo.lock` means patches only move on a deliberate `cargo update`.
- **§9.4.1 table**: change the OXC rows' "Version Policy" column from `exact pin`
  to `~` / `patch pin`.

### W2.4 Fix the over-strict / stale / obsolete version docs (the real cause of
"AI keeps flagging my unpinned versions")

- **`.github/copilot-instructions.md`**:
  - "Critical Version Pins" table — reframe as "reference versions + policy", not
    "verify before ANY code / most common source of errors". Fix stale numbers
    (`oxc 0.133.0`→`0.138.0`, `~0.133`→`~0.138`, `oxc_resolver ~11.19`→`~11.22`).
  - **Remove the npm `oxc-parser` row** — that package is banned (§9.4.4) and is
    not a dependency; listing it as a "critical pin" is wrong.
  - "Common Agent Mistakes" #6 (`oxc-parser 0.133.0 not 0.123.0`) — drop/fix.
- **`.agents/skills/project-scaffolding/SKILL.md`** — `~0.133`/`~11.19` → current;
  reconcile the `rust-version = "1.89.0"` MSRV snippet with the SRS "no fixed MSRV"
  stance (§9.4.3).
- **`.agents/skills/rust-module-graph-walker/SKILL.md`** (`0.133.0`/`v11.19.x`) and
  **`.agents/skills/rust-oxc-pipeline-runner/SKILL.md`** (`v0.133.0`) — update to
  `~0.138`/`~11.22` and the patch-pin language.
- **`.agents/skills/ts-oxc-parser-napi/SKILL.md`** — describes extension-host
  parsing via npm `oxc-parser`, which is banned/removed (parsing is daemon-side per
  §9.2). Verify against current extension source; if obsolete, delete the skill and
  its `copilot-instructions.md` skill-index row (or rewrite to the daemon reality).
- **`AGENTS.md` ↔ `.agents/rules/instructions.md` drift**: the rules copy is
  missing four lines the root has (no-deferred-work, no-superpower-doc-edits,
  no-unnecessary-tests, split-into-tasks). Resync so the two are identical
  (modulo the required frontmatter), and add a short pointer to the version policy.
- Add the tiered version policy itself somewhere authoritative (SRS §9 intro or a
  short subsection) so future agents apply it instead of defaulting to "pin all".

### W2.5 Verification for W2

`pnpm test:scripts` (dependency-policy + update-oxc-stack tests), `cargo build`
(resolves `~` cleanly, lock unchanged since `0.138.0` is already newest patch),
`node scripts/update-oxc-stack.mjs --dry-run <same version>` (round-trips with `~`),
and a grep proving no `0.133`/`~11.19`/npm-`oxc-parser` references remain.

## Commit structure

Per the granularity rule (one logical change per commit, NOT per plan step) and the
user's directive that version rules go in their own commit(s):

- **Commit 1 — `style: initial Biome formatting pass`** — mechanical reformat only,
  isolated so `git blame` stays clean. (Workstream 1, step 1a.)
- **Commit 2 — the quality-gates feature** — everything in Workstream 1: hooks,
  clippy wiring, deny tuning, commit-msg validator, CI backstops, changelog bodies,
  W1 docs. One cohesive commit.
- **Commit 3 — `build(deps): move the OXC stack to patch-only pins`** — Workstream 2
  §§W2.1–W2.3 (manifest + machinery + tests + SRS). Self-contained and green.
- **Commit 4 — `docs: align version-pinning guidance with the tiered policy`** —
  Workstream 2 §W2.4 (copilot-instructions, skills, AGENTS resync, obsolete-ref
  removal). Doc-only.

Commits 3 and 4 are separable and may be reordered/merged during implementation if
one turns out trivial; they must not fold into Commit 2. Each commit leaves the
full suite green.

## Risks / open points

- **Clippy debt volume** unknown until first run; may need triage into targeted
  `#[allow]`s with justification comments rather than fixes.
- **Biome initial reformat** touches many files — isolated in its own commit.
- **pnpm `prepare` + lefthook**: pnpm may require `lefthook` in
  `pnpm-workspace.yaml` `allowBuilds` (its postinstall is blocked otherwise); the
  explicit `prepare` script makes hook installation deterministic either way.
- **pre-push duration**: `pnpm test` compiles both languages (~minutes cold).
  Accepted per the request; can later be narrowed by changed-path detection.
- **OXC patch-drift bypasses the accuracy gate** (the deliberate reason it was
  exact-pinned). Mitigation: CI runs the accuracy suite on every push/PR, and the
  committed `Cargo.lock` means a patch only lands on an intentional `cargo update`.
  Documented in the SRS so the tradeoff is explicit, not silent.
- **`ts-oxc-parser-napi` obsolescence** must be confirmed against live extension
  source before deleting the skill — do not remove on inference alone.
