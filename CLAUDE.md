# AGENTS.md

Project instructions for agents working in this repository.

## Project Context

- Import Lens is a VS Code extension with a TypeScript extension host and a Rust daemon.
- Use `pnpm` as the npm package manager. Do not use `npm` or `yarn` for project scripts or dependency changes.
- Windows is the primary supported platform right now. Keep Windows compilation and packaging working before broadening to other targets.
- The SRS is the source of truth for intended behavior: `docs/ImportLens-SRS.md`.

## File And Formatting Rules

- Keep files in LF line endings. Never save files as CRLF.
- Keep edits scoped to the task. Do not mix unrelated refactors into feature or fix work.
- Keep types, constants, helpers, utilities, and UI code in their appropriate modules. Do not bury shared logic in unrelated files.
- Prefer existing patterns and helper modules before creating new abstractions.
- In TypeScript, prefer arrow functions where practical.
- Do not use double casting or unnecessary cast chains.

## Implementation Workflow

- **Default to the `hybrid-execution` skill for any multi-commit or multi-file piece of work.** Do not stop to offer "subagent-driven vs inline" as a choice; there is no better default. Implement inline — coding is interdependent, which is the case multi-agent handles worst. Spend subagent tokens on an independent review of the risky commits: anything touching the release path, a public API, a cache or data format, or a diff you cannot fully eyeball. Fan out only over genuinely independent strands. Treat a reviewer's findings as hypotheses — reproduce each against the code before fixing it, and decline the rest with a one-line reason. Deviate only with a stated reason.
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
// Drift: the expectation is derived. Bump the version in oxc-stack.config.mjs,
// forget daemon/Cargo.toml, and this fails. It knows something the file doesn't.
assert.match(cargoToml, new RegExp(`oxc_parser = "~${oxcStackConfig.currentOxcVersion}"`));

// Echo: the expectation is a literal. The only way to make this red is to edit
// the Dockerfile, at which point you already know what you changed.
assert.match(dockerfile, /^FROM node:24-bookworm$/mu);
```

**Classify before deleting.** Reading a config file does not make a test an Echo. Before deleting any test that opens a config, name its kind. If it is Drift, Property, or Guard, it stays.

**Prefer making drift impossible over testing for drift.** A Drift check is a consolation prize for two sources of truth you could not merge. Before writing one, try to delete the second source. Keep the check only where duplication is genuinely forced — `cli/importlens.mjs` and `extension/src/daemon/platform.ts` each redeclare `daemonRoot` because neither can import `scripts/targets.mjs` at runtime.

**Never assert a dependency version in a test, except oxc coordination.** oxc is the only dependency where a version bump can silently break the app. Do not assert versions of GitHub Actions, pnpm, Node, the Rust toolchain, `@types/vscode`, or any dev tooling. A break there is caught by CI before it ships.

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
- Follow Conventional Commits: `type(scope)!: subject` (<=72 chars, no trailing period). Types: `feat fix perf docs refactor style test chore ci build` (kept in sync with `cliff.toml`).
- A commit body (description) is REQUIRED and must explain the user-visible change and important technical rationale — it feeds the AI changelog. The `commit-msg` hook enforces this locally; CI enforces it on pull requests.
- Do not revert user changes unless explicitly asked.
- Before committing, check `git status --short` and review the staged diff.
- Hooks are lefthook-managed and installed by `pnpm install`. pre-commit runs Biome (auto-format + re-stage) and TypeScript check, plus clippy and cargo-deny for Rust changes; pre-push runs `pnpm test`. Bypass only in a genuine emergency with `--no-verify` (CI still enforces).
