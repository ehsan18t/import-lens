# Git Hooks, Commit Convention & Dependency-Version Policy — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add lefthook-managed pre-commit/commit-msg/pre-push hooks with Biome (TS lint+format) and clippy+cargo-deny (Rust) gates, enforce conventional commits with mandatory bodies, feed those bodies into the AI changelog, and separately move the OXC stack to patch-only pins while fixing the stale/over-strict version rules across the docs.

**Architecture:** Two workstreams shipped as four commits. Workstream 1 (Commits 1–2) adds tooling and hooks. Workstream 2 (Commits 3–4) applies the tiered dependency-version policy — OXC exact→patch-only with its enforcement machinery, then doc alignment. Each commit leaves the full suite green.

**Tech Stack:** lefthook (Go binary via npm), @biomejs/biome, clippy + `[workspace.lints]`, cargo-deny + cargo-deny-action, Node's built-in test runner, git-cliff, tsdown, pnpm.

**Source spec:** `docs/superpowers/specs/2026-07-04-git-hooks-and-commit-convention-design.md`

## Global Constraints

- **LF line endings only**; never CRLF. (AGENTS.md)
- **pnpm only** — never `npm`/`yarn` for project scripts or deps. (AGENTS.md) `npm view` for read-only version lookups is fine.
- **Windows is the primary platform** — every hook must run on Windows. lefthook's Go binary needs no POSIX shell.
- **Keep edits scoped**; do not mix unrelated refactors into a commit. (AGENTS.md)
- **Conventional-commit types** (single source, must equal `cliff.toml`): `feat fix perf docs refactor style test chore ci build`.
- **Dependency version policy** (tiered by upgrade blast radius): tier 1 track minor+patch (caret `^` / CI action major tag); tier 2 patch-only (tilde `~`) — OXC lives here after Commit 3; tier 3 exact (`=`) only when a patch can break. Add new deps at latest stable resolved at implementation time.
- **New tool versions at implementation time:** Biome `2.5.2`, lefthook `2.1.9` (verify with `npm view <pkg> version`; use whatever is latest then). Biome + lefthook are tier-1 → caret. cargo-deny-action → `@v2` major tag.
- **Full suite gate:** `pnpm test` (= `pnpm build && pnpm test:ts && pnpm test:scripts && pnpm test:rust`). Script tests run via `node --test "scripts/**/*.test.mjs"`.
- **OXC coordination invariant** (survives Commit 3): all 11 monorepo crates must resolve to ONE shared version; `oxc_resolver` is pinned independently; `oxc_mangler` must never appear.

---

# PART A — Commit 1: `style: initial Biome formatting pass`

Isolated mechanical reformat so `git blame` noise stays in one commit. Biome is added here; the whitespace-only reformat is committed alone.

### Task A1: Add Biome and its config

**Files:**
- Modify: `package.json` (devDependencies + scripts)
- Create: `biome.json`

- [ ] **Step 1: Add Biome as a dev dependency (tier-1 caret).**

Run (resolve latest, then add with caret):
```bash
BIOME_VER="$(npm view @biomejs/biome version)"
pnpm add -D "@biomejs/biome@^${BIOME_VER}"
```
Expected: `package.json` devDependencies gains `"@biomejs/biome": "^2.5.2"` (or newer).

- [ ] **Step 2: Create `biome.json`.**

```json
{
  "$schema": "https://biomejs.dev/schemas/2.5.2/schema.json",
  "vcs": { "enabled": true, "clientKind": "git", "useIgnoreFile": true },
  "files": {
    "includes": [
      "extension/**/*.{ts,mts}",
      "scripts/**/*.mjs",
      "cli/**/*.mjs",
      "*.{ts,mts,mjs,json}"
    ]
  },
  "formatter": { "enabled": true, "useEditorconfig": true },
  "linter": {
    "enabled": true,
    "rules": { "recommended": true }
  },
  "javascript": { "formatter": { "quoteStyle": "double" } }
}
```
Notes: `$schema` version must match the installed Biome (tier-3 rationale: schema must match binary). `useEditorconfig: true` keeps `.editorconfig` (LF, indent) authoritative. `useIgnoreFile` respects `.gitignore` so `dist/`, `target/`, `node_modules/` are skipped.

- [ ] **Step 3: Add convenience scripts to `package.json`.**

Add to `"scripts"`:
```json
"lint": "biome check",
"lint:fix": "biome check --write",
"format": "biome format --write"
```

- [ ] **Step 4: Verify Biome sees the right files and nothing external.**

Run: `pnpm exec biome check --files-ignore-unknown=true . | tail -20`
Expected: it reports formatting/lint diffs ONLY under `extension/`, `scripts/`, `cli/`, and root config files — never under `node_modules/`, `dist/`, `target/`.

### Task A2: Apply the reformat and commit it alone

**Files:** many (whitespace only).

- [ ] **Step 1: Format only (no lint fixes yet), to keep this commit whitespace-only.**

Run: `pnpm exec biome format --write .`

- [ ] **Step 2: Review that the diff is whitespace/formatting only.**

Run: `git diff --stat` then spot-check `git diff` on 2–3 files.
Expected: indentation/quote/semicolon/newline changes only; no logic changes. If Biome wants to change quotes in a way you dislike, adjust `javascript.formatter` in `biome.json` and re-run before committing.

- [ ] **Step 3: Confirm the build + tests still pass after reformat.**

Run: `pnpm check && pnpm test:ts && pnpm test:scripts`
Expected: PASS (formatting must not change behavior).

- [ ] **Step 4: Commit the reformat + Biome config together.**

```bash
git add biome.json package.json pnpm-lock.yaml
git add -A -- extension scripts cli tsdown.config.ts tsconfig.json tsconfig.test.json
git commit -m "style: adopt Biome and apply the initial formatting pass" -m "Adds @biomejs/biome (linter+formatter) with biome.json deferring to .editorconfig, and applies a whitespace-only format pass across extension/, scripts/, and cli/ so later feature commits stay free of reformat noise. No behavioral change."
```
Expected: one commit; `git blame` churn isolated here.

---

# PART B — Commit 2: Quality gates & commit convention

Everything in Workstream 1 except the isolated reformat. Build in the order below; commit once at the end (Task B9).

### Task B1: Wire clippy into the toolchain and workspace lints

**Files:**
- Modify: `rust-toolchain.toml`
- Modify: `Cargo.toml` (workspace lints)
- Modify: `daemon/Cargo.toml` (inherit lints)
- Modify: `clippy.toml` (fix macro paths)

**Interfaces:**
- Produces: a repo where `cargo clippy --workspace --all-targets -- -D warnings` is a meaningful gate and the sample `clippy.toml` thresholds actually fire.

- [ ] **Step 1: Add the clippy component.**

In `rust-toolchain.toml`, change:
```toml
components = ["rustfmt"]
```
to:
```toml
components = ["rustfmt", "clippy"]
```

- [ ] **Step 2: Add workspace lints to root `Cargo.toml`.**

Append after the `[profile.release]` block:
```toml
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
too_many_lines = "warn"
cognitive_complexity = "warn"
```
Rationale: `all` (correctness/suspicious/style/complexity/perf) at warn; `too_many_lines` + `cognitive_complexity` are the pedantic/nursery lints that activate the existing `clippy.toml` thresholds. Deliberately NOT enabling full `pedantic`/`nursery` (noise).

- [ ] **Step 3: Make the daemon inherit workspace lints.**

In `daemon/Cargo.toml`, add at the end:
```toml
[lints]
workspace = true
```

- [ ] **Step 4: Fix the disallowed-macro paths in `clippy.toml`.**

`todo!`/`unimplemented!` are defined in `core`. Change the two paths:
```toml
disallowed-macros = [
    { path = "std::dbg", reason = "Debug macro left in code - remove before committing" },
    { path = "core::todo", reason = "Unfinished code - implement before committing" },
    { path = "core::unimplemented", reason = "Unfinished code - implement before committing" },
]
```

- [ ] **Step 5: Verify the disallowed-macro paths are correct empirically.**

Temporarily add `dbg!(1); todo!();` into a daemon fn, then run:
`cargo clippy -p import-lens-daemon 2>&1 | grep -E 'disallowed|dbg|todo'`
Expected: clippy flags BOTH `dbg!` and `todo!`. Then revert the temporary edit. If `todo!` is NOT flagged, the path is wrong — try `std::todo` and re-verify.

- [ ] **Step 6: Run clippy and drive it to zero warnings.**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: initially may FAIL with real findings. Fix each finding properly. Where a lint is a false positive or not worth changing, add a scoped `#[allow(clippy::<lint>)]` with a one-line justification comment — never a blanket crate-level allow. Re-run until PASS (exit 0).

- [ ] **Step 7: Confirm formatting still clean.**

Run: `cargo fmt --check`
Expected: PASS. (If clippy fixes touched formatting, run `cargo fmt` first.)

### Task B2: Make cargo-deny pass against the real dependency tree

**Files:**
- Modify: `deny.toml`

- [ ] **Step 1: Run cargo-deny against the actual tree.**

Run: `cargo deny check 2>&1 | tail -40`
Expected: likely FAILS on the license allowlist (crates licensed BSD-3-Clause/ISC/etc. not yet allowed) and/or the `unicode-ident` clarify block.

- [ ] **Step 2: Tune `deny.toml` until clean.**

- Add any genuinely-present licenses to `licenses.allow` (verify each is acceptable — e.g. `BSD-3-Clause`, `ISC`, `MIT-0`). Do not blanket-allow.
- If `unicode-ident` now resolves as `Unicode-3.0` (already allowed), delete the stale `[[licenses.clarify]]` block for it. Verify with `cargo tree -i unicode-ident` and checking the crate's actual license before removing.
- Add `[[bans.skip]]` entries only for real duplicate-version warnings surfaced by the run.

- [ ] **Step 3: Re-run to green.**

Run: `cargo deny check`
Expected: PASS (advisories, licenses, bans, sources all OK).

### Task B3: Commit-message validator — failing tests first

**Files:**
- Create: `scripts/check-commit-msg.mjs`
- Create: `scripts/test/check-commit-msg.test.mjs`

**Interfaces:**
- Produces: `export const COMMIT_TYPES` (string[]), `export const validateCommitMessage(raw: string): { ok: boolean, errors: string[] }`. Consumed by the commit-msg hook (Task B6) and the CI commit-lint job (Task B7).

- [ ] **Step 1: Write the failing test file.**

```js
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { COMMIT_TYPES, validateCommitMessage } from "../check-commit-msg.mjs";

const ok = (raw) => validateCommitMessage(raw).ok;

test("accepts a conventional header with a real body", () => {
  assert.equal(ok("feat(hooks): add lefthook config\n\nWires pre-commit and pre-push jobs for both languages."), true);
});

test("accepts an optional scope-less header and a breaking-change bang", () => {
  assert.equal(ok("fix: correct off-by-one\n\nThe loop dropped the final import; now inclusive."), true);
  assert.equal(ok("refactor(ipc)!: change the frame header\n\nBumps the wire protocol; documented in the SRS."), true);
});

test("rejects an unknown type", () => {
  const { ok: pass, errors } = validateCommitMessage("wip(x): stuff\n\nA sufficiently long body goes here.");
  assert.equal(pass, false);
  assert.ok(errors.some((e) => /type/i.test(e)));
});

test("rejects a missing body (the description requirement)", () => {
  const { ok: pass, errors } = validateCommitMessage("docs: tidy readme");
  assert.equal(pass, false);
  assert.ok(errors.some((e) => /body/i.test(e)));
});

test("rejects a body that is only whitespace or too short", () => {
  assert.equal(ok("docs: tidy readme\n\n   "), false);
  assert.equal(ok("docs: tidy readme\n\ntoo short"), false);
});

test("rejects a subject with a trailing period", () => {
  assert.equal(ok("fix: do the thing.\n\nA sufficiently long body goes here to satisfy the rule."), false);
});

test("rejects a header longer than 72 characters", () => {
  const longSubject = "feat: " + "x".repeat(80);
  assert.equal(ok(longSubject + "\n\nA sufficiently long body goes here to satisfy the rule."), false);
});

test("requires a blank line between header and body", () => {
  assert.equal(ok("feat: add thing\nBody with no blank line separator that is long enough."), false);
});

test("passes through merge, revert, fixup and squash commits", () => {
  assert.equal(ok('Merge branch \"main\" into feature'), true);
  assert.equal(ok('Revert \"feat: add thing\"\n\nThis reverts commit abc123.'), true);
  assert.equal(ok("fixup! feat: add thing"), true);
  assert.equal(ok("squash! feat: add thing"), true);
});

test("ignores comment lines and the commit -v scissors diff", () => {
  const raw = [
    "feat(hooks): add lefthook config",
    "",
    "Wires pre-commit and pre-push jobs for both languages.",
    "# Please enter the commit message for your changes.",
    "# ------------------------ >8 ------------------------",
    "diff --git a/x b/x",
    "+noise that must not count as body",
  ].join("\n");
  assert.equal(ok(raw), true);
});

test("COMMIT_TYPES stays in sync with cliff.toml commit_parsers", () => {
  const cliff = readFileSync(new URL("../../cliff.toml", import.meta.url), "utf8");
  for (const type of COMMIT_TYPES) {
    assert.ok(
      new RegExp(`\\^${type}\\b`).test(cliff),
      `cliff.toml has no parser for type '${type}'`,
    );
  }
});
```

- [ ] **Step 2: Run it and watch it fail (module missing).**

Run: `node --test scripts/test/check-commit-msg.test.mjs`
Expected: FAIL — cannot find module `../check-commit-msg.mjs`.

### Task B4: Commit-message validator — implementation

**Files:**
- Create: `scripts/check-commit-msg.mjs`

- [ ] **Step 1: Implement the validator + thin CLI.**

```js
#!/usr/bin/env node

// Validates a commit message against the project's conventional-commit rules:
// `type(scope)!: subject` header (<=72 chars, no trailing period) plus a
// mandatory body. Machine-generated commits (merge/revert/fixup/squash) pass
// through. COMMIT_TYPES is the single source of truth and is asserted in tests
// to match cliff.toml so the changelog grouping and this gate never drift.

import { readFileSync } from "node:fs";

export const COMMIT_TYPES = [
  "feat", "fix", "perf", "docs", "refactor",
  "style", "test", "chore", "ci", "build",
];

const HEADER_MAX = 72;
const BODY_MIN_CHARS = 20;
const PASSTHROUGH = /^(Merge |Revert |fixup!|squash!)/u;

/** Strip git comment lines and everything from the `commit -v` scissors line. */
const stripNoise = (raw) => {
  const out = [];
  for (const line of raw.split("\n")) {
    if (/^# -+ >8 -+/u.test(line)) break;
    if (line.startsWith("#")) continue;
    out.push(line);
  }
  return out;
};

export const validateCommitMessage = (raw) => {
  const errors = [];
  const lines = stripNoise(raw ?? "");
  const header = (lines[0] ?? "").trimEnd();

  if (PASSTHROUGH.test(header)) return { ok: true, errors };

  const headerPattern = new RegExp(`^(${COMMIT_TYPES.join("|")})(\\([a-z0-9.-]+\\))?(!)?: (.+)$`, "u");
  const match = header.match(headerPattern);
  if (!match) {
    errors.push(
      `Header must be '<type>(<scope>)!: <subject>' where type is one of: ${COMMIT_TYPES.join(", ")}.`,
    );
  } else {
    const subject = match[4];
    if (header.length > HEADER_MAX) errors.push(`Header is ${header.length} chars; keep it <= ${HEADER_MAX}.`);
    if (subject.endsWith(".")) errors.push("Subject must not end with a period.");
  }

  if (lines.length > 1 && lines[1].trim() !== "") {
    errors.push("Leave a blank line between the header and the body.");
  }
  const body = lines.slice(2).join("\n").replace(/\s/gu, "");
  if (body.length < BODY_MIN_CHARS) {
    errors.push(
      `A commit body (description) is required — at least ${BODY_MIN_CHARS} non-whitespace characters explaining what and why.`,
    );
  }

  return { ok: errors.length === 0, errors };
};

const main = () => {
  const file = process.argv[2];
  if (!file) {
    console.error("Usage: node scripts/check-commit-msg.mjs <path-to-commit-msg-file>");
    process.exit(2);
  }
  const raw = readFileSync(file, "utf8");
  const { ok, errors } = validateCommitMessage(raw);
  if (!ok) {
    console.error("✖ Commit message rejected:\n");
    for (const e of errors) console.error(`  - ${e}`);
    console.error(
      "\nFormat: <type>(<scope>)!: <subject>\\n\\n<body>\nTypes: " + COMMIT_TYPES.join(", ") +
      "\nBypass in a genuine emergency with --no-verify.",
    );
    process.exit(1);
  }
};

// Run only as a CLI, not when imported by tests.
if (import.meta.url === `file://${process.argv[1]}` || process.argv[1]?.endsWith("check-commit-msg.mjs")) {
  main();
}
```

- [ ] **Step 2: Run the tests to green.**

Run: `node --test scripts/test/check-commit-msg.test.mjs`
Expected: PASS (all tests).

- [ ] **Step 3: Sanity-check the CLI both ways.**

```bash
printf 'feat(x): good subject\n\nA real body explaining the change in enough detail.' > /tmp/m1 && node scripts/check-commit-msg.mjs /tmp/m1; echo "exit=$?"
printf 'bad message' > /tmp/m2 && node scripts/check-commit-msg.mjs /tmp/m2; echo "exit=$?"
```
Expected: first prints nothing, `exit=0`; second prints errors, `exit=1`.

### Task B5: Commit message template

**Files:**
- Create: `.gitmessage`

- [ ] **Step 1: Create `.gitmessage`.**

```
# <type>(<scope>): <subject>   (<=72 chars, no trailing period)
#
# <body: required — explain WHAT changed and WHY, wrapped ~72 cols.
#  This becomes AI changelog input, so be specific and user-facing.>
#
# Types: feat fix perf docs refactor style test chore ci build
# Add ! after type/scope for a breaking change, e.g. feat(api)!: ...
```
(Wired via `git config commit.template` in Task B6.)

### Task B6: lefthook config and installation

**Files:**
- Modify: `package.json` (devDependency + `prepare` script)
- Create: `lefthook.yml`

**Interfaces:**
- Consumes: `scripts/check-commit-msg.mjs` (Task B4).
- Produces: installed `.git/hooks` after `pnpm install`.

- [ ] **Step 1: Add lefthook (tier-1 caret) and a prepare script.**

```bash
LEFTHOOK_VER="$(npm view lefthook version)"
pnpm add -D "lefthook@^${LEFTHOOK_VER}"
```
Then add to `package.json` `"scripts"`:
```json
"prepare": "lefthook install && git config commit.template .gitmessage"
```

- [ ] **Step 2: Create `lefthook.yml`.**

```yaml
# Local quality gates. Bypass in a genuine emergency with --no-verify (CI still enforces).
pre-commit:
  parallel: true
  jobs:
    - name: rust-fmt
      glob: "*.rs"
      run: cargo fmt
      stage_fixed: true
    - name: rust-clippy
      glob: ["*.rs", "Cargo.toml", "Cargo.lock", "clippy.toml"]
      run: cargo clippy --workspace --all-targets -- -D warnings
    - name: rust-deny
      glob: ["Cargo.toml", "Cargo.lock", "deny.toml"]
      run: cargo deny check
    - name: ts-biome
      glob: ["*.ts", "*.mts", "*.mjs", "*.json"]
      run: pnpm exec biome check --write --no-errors-on-unmatched --files-ignore-unknown=true {staged_files}
      stage_fixed: true
    - name: ts-typecheck
      glob: ["*.ts", "*.mts", "tsconfig*.json"]
      run: pnpm check

commit-msg:
  jobs:
    - name: commit-convention
      run: node scripts/check-commit-msg.mjs {1}

pre-push:
  jobs:
    - name: build-and-test
      run: pnpm test
```
Notes: `stage_fixed: true` re-stages formatter output → "format and add, never block". `rust-deny`/`rust-clippy` only fire when Rust/manifest files are staged, so TS-only commits skip Rust compilation. `{staged_files}` scopes Biome to staged paths; `{1}` is the commit-msg file path.

- [ ] **Step 3: Install hooks.**

Run: `pnpm exec lefthook install`
Expected: prints that pre-commit, commit-msg, pre-push hooks were synced; `.git/hooks/pre-commit` etc. now exist.

- [ ] **Step 4: Verify commit-msg hook rejects a bad message on Windows.**

```bash
git commit --allow-empty -m "bad message" || echo "correctly rejected"
```
Expected: the commit is REJECTED by `commit-convention` (exit non-zero, error printed). Confirms the hook runs on the primary platform.

- [ ] **Step 5: Verify a good message + auto-format passes.**

Make a trivial deliberately-misformatted TS edit, stage it, then:
```bash
git commit -m "test(hooks): verify the pre-commit and commit-msg gates" -m "Exercises Biome auto-format re-staging and the commit-message validator end to end."
```
Expected: Biome reformats and re-stages the file, the commit succeeds, and the committed file is formatted. Then reset this throwaway commit: `git reset --soft HEAD~1` (keep it out of Commit 2 unless it was a real change).

### Task B7: CI backstops

**Files:**
- Modify: `.github/workflows/validate.yml`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `scripts/check-commit-msg.mjs`.

- [ ] **Step 1: Add clippy + cargo-deny + biome to `validate.yml`.**

After the existing `Check Rust formatting` step (`cargo fmt --check`), add:
```yaml
      - name: Clippy
        run: cargo clippy --workspace --all-targets -- -D warnings

      - name: cargo-deny
        uses: EmbarkStudios/cargo-deny-action@v2

      - name: Biome CI
        run: pnpm exec biome ci .
```
And add `clippy` to the toolchain install step so the component is present:
```yaml
      - name: Install Rust
        run: |
          rustup toolchain install stable --profile minimal
          rustup default stable
          rustup component add clippy
```
(rustfmt already comes from `rust-toolchain.toml`; add clippy explicitly for the `-D warnings` gate.)

- [ ] **Step 2: Add a PR-only commit-message lint job to `ci.yml`.**

Append a second job (runs only on pull_request, where the range is well-defined):
```yaml
  commit-messages:
    name: Commit messages
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - name: Checkout
        uses: actions/checkout@v7.0.0
        with:
          fetch-depth: 0
      - name: Install Node.js
        uses: actions/setup-node@v6.4.0
        with:
          node-version: 24
      - name: Validate PR commit messages
        env:
          BASE_SHA: ${{ github.event.pull_request.base.sha }}
          HEAD_SHA: ${{ github.event.pull_request.head.sha }}
        run: |
          fail=0
          for sha in $(git rev-list "$BASE_SHA".."$HEAD_SHA"); do
            msg="$(git log -1 --format=%B "$sha")"
            printf '%s' "$msg" > /tmp/msg
            if ! node scripts/check-commit-msg.mjs /tmp/msg; then
              echo "  ^ commit $sha"
              fail=1
            fi
          done
          exit $fail
```

- [ ] **Step 3: Lint the workflow YAML.**

Run: `node --test scripts/test/validate-workflow.test.mjs scripts/test/ci-workflow.test.mjs`
Expected: PASS. If these tests assert exact step lists, update their expected content to include the new steps/job in the same task.

### Task B8: Feed commit bodies into the AI changelog

**Files:**
- Modify: `scripts/generate-changelog.mjs`
- Modify: `scripts/test/generate-changelog.test.mjs`

**Interfaces:**
- Modifies: `collectSubjects` → also collect bodies; `buildUserPrompt` → include truncated bodies.

- [ ] **Step 1: Update/extend the failing test first.**

In `scripts/test/generate-changelog.test.mjs`, add a test asserting the AI user prompt includes body text (adapt to the file's existing style/exports):
```js
test("buildUserPrompt includes truncated commit bodies", () => {
  const prompt = buildUserPrompt("1.0.0", [
    { subject: "feat(x): add thing", body: "Adds the thing so users can do Y." },
  ]);
  assert.match(prompt, /add thing/);
  assert.match(prompt, /Adds the thing so users can do Y\./);
});
```
Run: `node --test scripts/test/generate-changelog.test.mjs`
Expected: FAIL (buildUserPrompt still takes plain subject strings).

- [ ] **Step 2: Collect subject+body records.**

Replace `collectSubjects` with a record collector using a NUL delimiter:
```js
/** Commit {subject, body} records in the range, excluding merges. */
const collectCommits = (range) => {
  const result = runCapture("git", ["log", range, "--no-merges", "--pretty=format:%s%n%b%x00"]);
  if (result.status !== 0) {
    throw new Error(`git log failed: ${result.stderr?.trim() ?? "unknown error"}`);
  }
  return result.stdout
    .split("\0")
    .map((rec) => rec.replace(/^\n+/u, "").trim())
    .filter((rec) => rec.length > 0)
    .map((rec) => {
      const [subject, ...rest] = rec.split("\n");
      return { subject: subject.trim(), body: rest.join("\n").trim() };
    });
};
```

- [ ] **Step 3: Include bodies (truncated) in the AI prompt.**

Update `buildUserPrompt` to accept records and cap each body:
```js
const BODY_CAP = 600;
export const buildUserPrompt = (version, commits) =>
  [
    `Release version: ${version}`,
    "",
    "Commits (subject, then body detail):",
    ...commits.map(({ subject, body }) => {
      const trimmed = body ? body.slice(0, BODY_CAP) : "";
      return trimmed ? `- ${subject}\n  ${trimmed.replace(/\n/gu, "\n  ")}` : `- ${subject}`;
    }),
  ].join("\n");
```
Update `SYSTEM_PROMPT` to add: `"- Use the body detail to write clearer, user-facing bullets, but never invent changes absent from the commits."`

- [ ] **Step 4: Keep the git-cliff / plain fallbacks subject-only.**

Ensure `renderPlainChangelog` and the git-cliff path still use `commits.map((c) => c.subject)` where they previously used subject strings. Update `main()` to call `collectCommits` and pass records to `buildUserPrompt`, subjects to the fallbacks.

- [ ] **Step 5: Run the changelog tests to green.**

Run: `node --test scripts/test/generate-changelog.test.mjs`
Expected: PASS.

### Task B9: W1 docs + commit the whole feature

**Files:**
- Modify: `AGENTS.md`, `.agents/rules/instructions.md` (keep them in sync)
- Modify: `README.md`

- [ ] **Step 1: Document commit rules + hooks in `AGENTS.md` "Git Expectations".**

Add bullets:
```markdown
- Follow Conventional Commits: `type(scope)!: subject` (<=72 chars, no trailing period). Types: feat fix perf docs refactor style test chore ci build.
- A commit body (description) is REQUIRED and should explain what changed and why — it feeds the AI changelog. `commit-msg` enforces this locally; CI enforces it on PRs.
- Hooks are lefthook-managed and installed by `pnpm install`. pre-commit runs Biome (auto-format+re-stage) and clippy/cargo-deny for Rust; pre-push runs `pnpm test`. Bypass only in emergencies with `--no-verify`.
```
Mirror the same addition in `.agents/rules/instructions.md` (see Task D3 for the full resync; here just keep them from diverging further).

- [ ] **Step 2: Add a contributor note to `README.md`.**

A short "Contributing / Development" note: `pnpm install` wires the hooks; list the pre-commit/commit-msg/pre-push gates; how to run each manually (`pnpm lint`, `cargo clippy ...`, `cargo deny check`, `pnpm test`); `--no-verify` is for emergencies only.

- [ ] **Step 3: Full-suite gate.**

Run: `pnpm test`
Expected: PASS (build + TS + scripts + rust). Fix anything red before committing.

- [ ] **Step 4: Commit the feature.**

```bash
git add rust-toolchain.toml Cargo.toml daemon/Cargo.toml clippy.toml deny.toml \
        lefthook.yml .gitmessage package.json pnpm-lock.yaml \
        scripts/check-commit-msg.mjs scripts/test/check-commit-msg.test.mjs \
        scripts/generate-changelog.mjs scripts/test/generate-changelog.test.mjs \
        .github/workflows/validate.yml .github/workflows/ci.yml \
        AGENTS.md .agents/rules/instructions.md README.md
# plus any daemon/*.rs clippy fixes
git add -A -- daemon/src
git commit -m "feat(dx): add git hooks, lint gates, and enforced commit convention" -m "$(cat <<'EOF'
lefthook now manages pre-commit (Biome auto-format+re-stage and TS typecheck;
clippy and cargo-deny for Rust, scoped to changed files), commit-msg
(conventional commits with a mandatory body via scripts/check-commit-msg.mjs,
type list kept in sync with cliff.toml by test), and pre-push (pnpm test).
clippy is wired through [workspace.lints] so the sample clippy.toml thresholds
finally fire, and deny.toml is tuned to pass against the real tree. CI gains
clippy/cargo-deny/biome checks and a PR commit-message lint. The AI changelog
now consumes commit bodies, not just subjects, which is the whole reason bodies
are mandatory.
EOF
)"
```

- [ ] **Step 5: Re-verify hooks fire on the real commit path** (already exercised in B6). Confirm `git log -1 --format=%B` shows subject+body.

---

# PART C — Commit 3: `build(deps): move the OXC stack to patch-only pins`

Move OXC from exact (`=`) to patch-only (`~`), migrating the enforcement machinery, tests, and SRS in lockstep. TDD: update the enforcement tests first (they encode the contract), watch them fail, then change the code + manifest to satisfy them.

### Task C1: Retarget the dependency-policy test to `~`

**Files:**
- Modify: `scripts/test/dependency-policy.test.mjs`

- [ ] **Step 1: Change the OXC pin assertions from `=` to `~`.**

In the first test, replace the crate + resolver assertions:
```js
  for (const crate of oxcStackConfig.oxcCrates) {
    assert.match(cargoToml, new RegExp(`^${crate} = "~${escapedVersion(oxcStackConfig.currentOxcVersion)}"$`, "mu"));
  }

  assert.doesNotMatch(cargoToml, /^oxc_mangler = /mu);
  assert.match(cargoToml, new RegExp(`^oxc_resolver = "~${escapedVersion(oxcStackConfig.currentResolverVersion)}"$`, "mu"));
```
Leave every other assertion (mangler absence, dockerfile ARGs, rust-version absence, pnpm/node versions, `oxc-parser` undefined, deps:update scripts) unchanged.

- [ ] **Step 2: Run it and watch it fail (manifest still uses `=`).**

Run: `node --test scripts/test/dependency-policy.test.mjs`
Expected: FAIL — cargoToml still has `= "=0.138.0"`.

### Task C2: Migrate the enforcement helper to patch-pins

**Files:**
- Modify: `scripts/oxc-stack-helpers.mjs`

- [ ] **Step 1: Change `validateCurrentStack` to require `~`.**

Replace the exact-pin needle and the resolver check:
```js
  const crateVersions = oxcStackConfig.oxcCrates.map((crate) => {
    const match = cargoToml.match(new RegExp(`^${crate}\\s*=\\s*"(~[^"]+)"$`, "mu"));
    if (!match) {
      throw new Error(`Missing patch-pin (~) for OXC crate: ${crate}`);
    }
    return match[1].slice(1);
  });
  const uniqueCrateVersions = new Set(crateVersions);
  if (uniqueCrateVersions.size !== 1) {
    throw new Error(`Current OXC crate versions are not coordinated: ${[...uniqueCrateVersions].join(", ")}`);
  }

  if (!/^oxc_resolver\s*=\s*"~[^"]+"$/mu.test(cargoToml)) {
    throw new Error("Missing patch-pin (~) for oxc_resolver");
  }
```
The coordination invariant is preserved (all monorepo crates must share one `~`-version).

- [ ] **Step 2: Change `updateCargoToml` to write `~`.**

```js
export const updateCargoToml = (cargoToml, oxcVersion, resolverVersion) => {
  let next = cargoToml;
  for (const crate of oxcStackConfig.oxcCrates) {
    next = next.replace(new RegExp(`^${crate}\\s*=\\s*"[^"]+"$`, "gmu"), `${crate} = "~${oxcVersion}"`);
  }
  return next.replace(/^oxc_resolver\s*=\s*"[^"]+"$/gmu, `oxc_resolver = "~${resolverVersion}"`);
};
```
(`replaceKnownVersions` needs no change — it swaps version numbers, leaving the `~` prefix intact.)

### Task C3: Flip the manifest to patch-pins

**Files:**
- Modify: `daemon/Cargo.toml`

- [ ] **Step 1: Change all OXC pins `=` → `~`.**

Replace each `oxc_* = "=0.138.0"` with `oxc_* = "~0.138.0"` (all 11 monorepo crates) and `oxc_resolver = "=11.22.0"` → `oxc_resolver = "~11.22.0"`.
(Optional cosmetic: leave non-OXC lines untouched.)

- [ ] **Step 2: Verify Cargo resolves `~` cleanly with no lockfile churn.**

Run: `cargo build -p import-lens-daemon`
Expected: builds; `git diff Cargo.lock` shows NO changes (0.138.0 is already the newest 0.138.x, so `~0.138.0` resolves identically).

- [ ] **Step 3: Run the dependency-policy test to green.**

Run: `node --test scripts/test/dependency-policy.test.mjs`
Expected: PASS.

### Task C4: Fix the updater fixtures and round-trip test

**Files:**
- Modify: `scripts/test/update-oxc-stack.test.mjs`

- [ ] **Step 1: Update the in-test fixtures to use `~`.**

In the fixture helpers (`cargoTomlFixture`, and the SRS fixture if it embeds pins), change OXC crate lines from `= "=<ver>"` to `= "~<ver>"` so they satisfy the migrated `validateCurrentStack`. The `replaceKnownVersions` test content (`| oxc_parser | <ver> | exact pin |`) tests version-token replacement only and can keep its literal text, but for consistency change the fixture's SRS wording to `patch pin` where the updater re-renders it.

- [ ] **Step 2: Run the updater tests to green.**

Run: `node --test scripts/test/update-oxc-stack.test.mjs`
Expected: PASS. If a test asserts the updater WROTE `= "=<ver>"`, update the expected string to `= "~<ver>"`.

- [ ] **Step 3: Dry-run the real updater to confirm a clean round-trip.**

Run: `node scripts/update-oxc-stack.mjs --oxc 0.138.0 --resolver 11.22.0 --dry-run`
Expected: reports no/ minimal edits and does NOT reintroduce `=` pins; exits 0.

### Task C5: Update the SRS to document patch-only OXC

**Files:**
- Modify: `docs/ImportLens-SRS.md` (§9.3, §9.4.1)

- [ ] **Step 1: Rewrite §9.3's exact-pin mandate.**

Replace the sentence requiring `=0.138.0` exact syntax with the patch-pin policy: OXC monorepo crates use `~<version>` (patch-float, coordinated to one shared minor.patch); `oxc_resolver` uses its own independent `~`; minor/major jumps remain a deliberate `pnpm deps:update:oxc` batch gated by the accuracy suite; patch bumps flow automatically but are still caught by CI's accuracy run and only move on a deliberate `cargo update` (committed `Cargo.lock`). Keep the `oxc_mangler`-must-not-return rule.

- [ ] **Step 2: Update §9.4.1 policy cells.**

Change the OXC rows' "Version Policy" column from `exact pin` to `~` (patch pin). Update the audit-date line to 2026-07-04.

- [ ] **Step 3: Commit Part C.**

```bash
git add daemon/Cargo.toml scripts/oxc-stack-helpers.mjs \
        scripts/test/dependency-policy.test.mjs scripts/test/update-oxc-stack.test.mjs \
        docs/ImportLens-SRS.md
git commit -m "build(deps): move the OXC stack to patch-only pins" -m "$(cat <<'EOF'
OXC ceases to be an exact-pin exception and joins the tiered version policy at
tier 2 (patch-only). All 11 monorepo crates and oxc_resolver move from = to ~,
and the enforcement machinery moves with them: oxc-stack-helpers now validates
and writes ~ pins (keeping the coordinated-version invariant), the dependency-
policy and updater tests assert ~, and SRS 9.3/9.4.1 document the patch-float
policy. Cargo.lock is unchanged because 0.138.0 is already the newest patch.
Patch drift is caught by the CI accuracy suite and only lands on a deliberate
cargo update; minor/major jumps still go through pnpm deps:update:oxc.
EOF
)"
```
- [ ] **Step 4: Full suite gate.** Run `pnpm test`. Expected: PASS.

---

# PART D — Commit 4: `docs: align version-pinning guidance with the tiered policy`

Doc-only. Fixes the stale numbers, the obsolete npm `oxc-parser` references, the over-strict tone, and the AGENTS/rules drift — the real cause of AI flagging unpinned deps.

### Task D1: Fix `.github/copilot-instructions.md`

**Files:**
- Modify: `.github/copilot-instructions.md`

- [ ] **Step 1: Reframe the "Critical Version Pins" section.**

Retitle to "Reference Versions & Pinning Policy" and replace the alarmist lead ("verify before ANY code / most common source of agent errors") with a one-line pointer to the tiered policy (tier 1 caret, tier 2 tilde incl. OXC, tier 3 exact). Fix stale numbers: `oxc-parser 0.133.0` row → delete (banned/not a dep); `oxc_parser (Rust) ~0.133` → `~0.138`; `oxc_resolver ~11.19` → `~11.22`. Leave `redb ^4`, `papaya ~0.2`, pnpm/types rows.

- [ ] **Step 2: Fix "Common Agent Mistakes" #6.**

Remove or correct the `oxc-parser version 0.133.0, not 0.123.0` item (npm oxc-parser is banned).

- [ ] **Step 3: Update the Skill Index if the napi skill is removed (see D4).**

If `ts-oxc-parser-napi` is deleted, remove its row from the Skill Index table.

- [ ] **Step 4: Grep-verify no stale refs remain.**

Run: `grep -nE '0\\.133|11\\.19|oxc-parser' .github/copilot-instructions.md`
Expected: no matches (except, if kept, an explicit "banned" mention).

### Task D2: Fix the Rust OXC skill docs

**Files:**
- Modify: `.agents/skills/project-scaffolding/SKILL.md`
- Modify: `.agents/skills/rust-module-graph-walker/SKILL.md`
- Modify: `.agents/skills/rust-oxc-pipeline-runner/SKILL.md`

- [ ] **Step 1: Update project-scaffolding pins + MSRV.**

`~0.133`→`~0.138`, `~11.19`→`~11.22`; retitle "Pinned Versions" to note patch-only. Reconcile the `rust-version = "1.89.0"` snippet with the SRS "no fixed MSRV" stance (§9.4.3) — remove the fixed `rust-version` line to match `Cargo.toml` (the dependency-policy test asserts its absence).

- [ ] **Step 2: Update the other two skills' version mentions.**

`rust-module-graph-walker`: `0.133.0`/`v11.19.x` → `0.138.0`/`v11.22.x` and "patch-pinned (`~`), coordinated". `rust-oxc-pipeline-runner`: `v0.133.0` → `v0.138.0` (description + body).

- [ ] **Step 2b: Grep-verify.** Run `grep -rnE '0\\.133|11\\.19' .agents/skills/` → no matches.

### Task D3: Resync `AGENTS.md` ↔ `.agents/rules/instructions.md`

**Files:**
- Modify: `.agents/rules/instructions.md`

- [ ] **Step 1: Bring the rules copy in line with root AGENTS.md.**

Add the four missing bullets to `.agents/rules/instructions.md` "Implementation Workflow" (no-unnecessary-tests; do-it-now-not-deferred; no superpower-doc edits; split-into-tasks) plus the Git-Expectations commit-convention bullets from Task B9, so the body is identical to `AGENTS.md` (keeping only the required YAML frontmatter difference).

- [ ] **Step 2: Verify the bodies match.**

Run: `diff <(tail -n +6 .agents/rules/instructions.md) AGENTS.md`
Expected: no differences (or only a trailing-newline diff — normalize it).

### Task D4: Resolve the obsolete `ts-oxc-parser-napi` skill

**Files:**
- Possibly delete: `.agents/skills/ts-oxc-parser-napi/SKILL.md`
- Modify: `.github/copilot-instructions.md` skill index (if deleted)

- [ ] **Step 1: Confirm obsolescence against live source.**

Run: `grep -rnE 'oxc-parser|parseSync|staticImports' extension/src/`
Expected: NO matches (already verified — extension host does not parse; the daemon does). This confirms the skill (which mandates npm `oxc-parser` NAPI in the extension host) contradicts the current architecture and §9.4.4's ban.

- [ ] **Step 2: Delete the obsolete skill (only if Step 1 confirmed empty).**

Run: `git rm .agents/skills/ts-oxc-parser-napi/SKILL.md`
Then remove its row from the copilot-instructions Skill Index (Task D1 Step 3). If Step 1 unexpectedly finds usage, do NOT delete — instead rewrite the skill to the daemon-side reality and drop the npm-version mandate.

### Task D5: Add the tiered policy to an authoritative doc + commit

**Files:**
- Modify: `docs/ImportLens-SRS.md` (§9 intro)

- [ ] **Step 1: State the tiered policy once, authoritatively.**

Add a short paragraph at the top of §9 (or a §9.0) describing the three tiers and "add new deps at latest stable; stay current automatically wherever safe", so future agents apply it instead of defaulting to "pin everything". Cross-reference §9.3/§9.4.1.

- [ ] **Step 2: Commit Part D.**

```bash
git add .github/copilot-instructions.md .agents/skills docs/ImportLens-SRS.md .agents/rules/instructions.md
git commit -m "docs: align version-pinning guidance with the tiered policy" -m "$(cat <<'EOF'
Removes the over-strict 'pin everything / verify before any code' framing and
the stale/obsolete version references that made agents flag safe caret/tilde
deps as issues. Fixes OXC numbers (0.133->0.138, 11.19->11.22) and the patch-
pin language across copilot-instructions and the .agents Rust skills; deletes
the obsolete ts-oxc-parser-napi skill (extension host no longer parses; npm
oxc-parser is banned) and its skill-index row; resyncs .agents/rules copy with
AGENTS.md; and records the tiered version policy once in the SRS.
EOF
)"
```
- [ ] **Step 3: Final full-suite gate.** Run `pnpm test`. Expected: PASS.

---

## Self-Review notes (for the executor)

- **Spec coverage:** D1–D8 of the spec → Part B; W2.1–W2.3 → Part C; W2.4 → Part D. Biome reformat isolation → Part A. All four commits from § Commit structure are represented.
- **Sync invariant:** `COMMIT_TYPES` (B4) must match `cliff.toml` — asserted by the B3 sync test. The commit-msg hook (B6) and CI job (B7) both call the same `check-commit-msg.mjs` — no drift.
- **OXC coordination invariant** is preserved through C2 (helper still enforces one shared version). `Cargo.lock` must stay unchanged in C3 Step 2 — if it changes, a newer patch exists; that is fine but re-run the accuracy suite (`pnpm test:accuracy`) before committing.
- **Watch:** the `validate-workflow.test.mjs` / `ci-workflow.test.mjs` / `build-workflow.test.mjs` / `release-workflow.test.mjs` tests assert exact pinned action versions and step lists — Task B7 must update their expectations if they enumerate steps. Run them explicitly.
- **Do not** let Parts C/D fold into Part B's commit; the user requires version rules in their own commit(s).
```
