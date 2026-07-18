# AGENTS.md

Project instructions for agents working in this repository.

## Project Context

- Import Lens is a VS Code extension with a TypeScript extension host and a Rust daemon.
- Use `pnpm` as the npm package manager. Do not use `npm` or `yarn` for project scripts or dependency changes.
- Windows is the primary supported platform right now. Keep Windows compilation and packaging working before broadening to other targets.
- The SRS is the source of truth for intended behavior: `docs/ImportLens-SRS.md`.
- Known issues we have decided **not** to fix live in `docs/known-issues.md`. **Read it before reporting or "fixing" something** — it may already be a recorded, deliberate decision (some of them were "fixed" once and the fix was worse than the issue).

## Deciding What To Fix Now

A finding being **real** is not the same as it being **blocking**.

> **Fix it in the current piece of work only if it (a) shows the user a WRONG NUMBER, or (b) can WEDGE the system or lose data.**

Everything else gets an entry in `docs/known-issues.md` — stating what actually happens and why it is not fixed — and goes back in the queue. Add the entry when you **decide** not to fix something, not when you find it.

If a fix chains into a third round on the same sub-item, stop and re-check it against the plan. Do not let the next review report decide your priorities for you.

## File And Formatting Rules

- Keep files in LF line endings. Never save files as CRLF.
- Keep edits scoped to the task. Do not mix unrelated refactors into feature or fix work.
- Keep types, constants, helpers, utilities, and UI code in their appropriate modules. Do not bury shared logic in unrelated files.
- Prefer existing patterns and helper modules before creating new abstractions.
- In TypeScript, prefer arrow functions where practical.
- Do not use double casting or unnecessary cast chains.

## Keeping The Core Small

The asset feature reached ~9,000 lines before anyone counted. No single change was unreasonable; the
growth came from adding beside instead of replacing. These rules target that specific failure, and
each one has a trigger you can check in review rather than an aspiration.

- **One mechanism per concern. A second way to do something the codebase already does is a defect.**
  Four provider constructors, two read ledgers, and an `Option<Context>` that made production safety
  bypassable were all added this way — each one locally sensible, none of them replacing what it
  duplicated. If a variant is needed, change the existing thing; if it genuinely cannot bend, say in
  the commit why not.
- **Adding without deleting is the thing to be suspicious of.** A change that is pure addition should
  name what it replaced, or say why nothing could be. This is a prompt to look, not a prohibition.
- **No speculative surface.** An item with no caller is deleted, not kept for later. `AssetKind::ALL`
  and `as_str` sat unused through three reviews.
- **Tests use production entry points.** A test-only code path doubles the code and measures a system
  that does not ship. Supply test *data* and test *limits*, never a parallel path — the asset tests
  bypassed the resource ledger for weeks and hid a real production limit while doing it.
- **State that must be interpreted together belongs together.** Six correlated collections that three
  functions each re-read is the shape that produces both bloat and disagreement between surfaces.
- **Roughly 700 lines is where a source file needs a reason.** Not a hard limit and nothing enforces
  it; it is the point at which "should this be two things?" is worth asking out loud rather than
  drifting past. The daemon has several files well over it that grew there unnoticed.

**Performance: measure it, do not assert it.** "Optimized" is not a property you can review. Brotli
quality was assumed too slow to raise for a year; measuring took ten minutes and showed quality 11
costs 35x (disqualifying) while quality 9 halves the error for +33 ms — a real option nobody had
costed. Claims about speed or size in a commit message should carry the number that supports them.

**Where this stops.** Correctness outranks size, always. This product's whole value is a number the
user can trust, so never trade a disclosed-correct result, a freshness guarantee, or a gate for
fewer lines. Shorter code that is harder to verify is not smaller — the review cost moved, it did not
go away. When a reduction and a guarantee genuinely conflict, keep the guarantee and record the
larger size honestly.

## Orchestration Default

- **lean-orchestration is the default execution mode — no `/lean` needed.** Start every non-trivial task (feature, bug hunt, review/audit, design critique, refactor, mixed prompt) by invoking the `lean-orchestration` skill and following its routing. Step 0 of the skill still governs small work: quick lookups, tight debug loops, and small single-file changes stay inline. Full multi-agent/Workflow fan-outs only when explicitly requested. The skill and role agents live under `.claude/`; see `docs/lean-orchestration-setup.md`.

## Implementation Workflow

- Treat a reviewer's or subagent's findings as hypotheses — reproduce each against the code before fixing it.
- Read the relevant existing code before editing.
- Add or update tests for behavior changes and bug fixes.
- For daemon changes, run Rust formatting and tests.
- For extension changes, run TypeScript checks and tests.
- If behavior diverges from the SRS, update `docs/ImportLens-SRS.md` in the same task.
- If daemon code changes, rebuild/package for Windows and refresh the daemon hash before handing off.
- Don't put something I give you as future or milestone or deferred work. Because if I give some you to do, I am asking you to do it right now.
- Don't update docs inside the superpower sub-directory unless it's something that has not been implemented yet.
- Split work into tasks.

## Testing Policy

**A test earns its place only if it can fail when nobody edited the file it reads.**

Four kinds of test pass that rule:

- **Logic** — inputs through a function to outputs. Fails when you break the function.
- **Drift** — reads two independently-maintained sources and asserts they agree. Fails when you edit one and forget the other.
- **Property** — quantifies over *every* item in a set. Fails when someone adds a new item that violates the rule.
- **Guard** — asserts an anti-pattern is *absent*. Fails when someone reintroduces the bad thing.

Anything else is an **Echo**: a copy of a config file that throws when the copy goes stale. Echoes are banned. They cannot catch a bug; they can only tax an edit.

Drift and Echo look identical from a distance — both read a config and assert something about it. The discriminator is where the expected value comes from:

```js
// Drift: the expectation is derived. Bump the version in compiler-stack.config.mjs,
// forget daemon/Cargo.toml, and this fails. It knows something the file doesn't.
assert.match(cargoToml, new RegExp(`oxc_parser = "=${compilerStackConfig.currentOxcVersion}"`));

// Echo: the expectation is a literal. The only way to make this red is to edit
// the Dockerfile, at which point you already know what you changed.
assert.match(dockerfile, /^FROM node:24-bookworm$/mu);
```

**Classify before deleting.** Reading a config file does not make a test an Echo. Before deleting any test that opens a config, name its kind. If it is Drift, Property, or Guard, it stays.

**Prefer making drift impossible over testing for drift.** A Drift check is a consolation prize for two sources of truth you could not merge. Before writing one, try to delete the second source. Keep the check only where duplication is genuinely forced — `cli/importlens.mjs` and `extension/src/daemon/platform.ts` each redeclare `daemonRoot` because neither can import `scripts/targets.mjs` at runtime.

**Never assert a dependency version in a test, except the size-determining stack.** These are the only dependencies where a version bump can silently change measured output, so they are exact-pinned and version-tested: rolldown, the OXC monorepo crates, oxc_resolver, the `sideEffects` glob matcher (fast-glob), and the CSS processor (lightningcss). The first four are coordinated (their versions derive from rolldown's own graph, via `deps:update:compiler`); lightningcss is a STANDALONE exact-pin (its version is chosen independently and it is not reachable from rolldown/oxc, so it stays out of the fingerprint closure). Do not assert versions of GitHub Actions, pnpm, Node, the Rust toolchain, `@types/vscode`, or any dev tooling. A break there is caught by CI before it ships.

Do not:

- assert the literal text of a workflow, a Dockerfile, or a `package.json` field
- test a script that has no branches
- write a test whose expected value you typed by hand out of the file under test
- trust the name of a test as evidence of its kind — `performance-policy.test.mjs` announced that it protected the release performance entrypoint, and contained `assert.equal(manifest.scripts["test:rust"], "cargo test --workspace")`

Asserting a rendered UI string is **Logic**, not an Echo: the expected value is the output of a function under known inputs, not a line copied out of the file being read.

## Verification Commands

Use the narrowest relevant checks while developing, then run the full set before completion:

```powershell
pnpm check
pnpm test
cargo fmt --check
pnpm package:win32-x64
```

## Packaging Notes

- `pnpm package:win32-x64` rebuilds the daemon, copies the Windows binary, refreshes `extension/src/daemon/knownHashes.generated.ts`, builds the extension bundle, and creates the Windows VSIX.
- Generated build artifacts such as `dist/` (daemon binaries, extension bundle, VSIXes) and `target/` are ignored unless the repository policy changes.

## Git Expectations

- **One commit per logically-coherent change — NOT one commit per plan step.** A multi-step plan that delivers a single cohesive change lands as ONE commit, or a small handful when there are genuinely separable concerns (an unrelated bug found along the way, an isolated mass-reformat). Plan steps are an implementation order, not a commit boundary. Never split a cohesive change into micro-commits merely because the plan had that many tasks — and if a plan or skill template says to commit per task, this rule wins. When unsure how to split, ask, or default to fewer.
- Never commit to `main`. Branch first — including for design and plan documents.
- Follow Conventional Commits: `type(scope)!: subject` (<=72 chars, no trailing period). Types: `feat fix perf docs refactor style test chore ci build revert` (kept in sync with `cliff.toml`).
- A commit body (description) is REQUIRED and must explain the user-visible change and important technical rationale — it feeds the AI changelog. The `commit-msg` hook enforces this locally; CI enforces it on pull requests.
- **Do NOT hard-wrap commit message bodies.** Write each paragraph as one continuous line and separate paragraphs with a single blank line. Never insert line breaks inside a paragraph to fit an assumed column width; viewers and tooling soft-wrap. A body is a handful of single-line paragraphs, not a block of fixed-width lines. (Pass each paragraph as its own `-m` argument, which produces exactly this shape.)
- **A history rewrite bypasses the `commit-msg` hook.** `git commit-tree`, `filter-branch`, and non-interactive replay scripts write commits without running the hook, so nothing checks the new messages until CI (which validates every commit on the PR). Before moving the branch, self-verify every rewritten message — run `scripts/check-commit-msg.mjs` over each one, or confirm each subject header is `<=72` chars with a body and no hard-wrapping. Do NOT loosen the `<=72` limit to make a red check pass; shorten the subject. (Five over-length squash subjects reached CI exactly this way.)
- Do not revert user changes unless explicitly asked.
- Before committing, check `git status --short` and review the staged diff.
- Hooks are lefthook-managed and installed by `pnpm install`. pre-commit runs Biome (auto-format + re-stage) and TypeScript check, plus clippy and cargo-deny for Rust changes; pre-push runs `pnpm test`. Bypass only in a genuine emergency with `--no-verify` (CI still enforces).
