# ImportLens Codebase Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix verified build, CI, parser, IPC lifecycle, and unavailable-daemon issues without hiding the larger Rust tree-shaking gap behind estimator tweaks.

**Architecture:** Keep target metadata in one script module and make every build path consume that metadata. Add regression tests before behavior changes, keep platform packaging target-specific, and leave the documented OXC graph-walker implementation as a separate release-blocking project.

**Tech Stack:** TypeScript/Node test runner, pnpm, VS Code extension APIs, Rust daemon, Docker Compose, GitHub Actions.

---

## Verified Findings

1. Docker build fails in the builder container because `Dockerfile.cross` uses Rust 1.85.0 while the locked `redb 4.1.0` requires Rust 1.89 or newer. `docker compose up --build` still exits 0 because the package script does not propagate the builder service exit code.
2. Platform package scripts all run the host `cargo build --release`; cross-target packages can fail or copy the wrong binary. `scripts/copy-daemon.mjs` explicitly falls back to `target/release`, which makes this unsafe.
3. `scripts/package-vsix.mjs` shells out to `npm install`, violating the repository rule to use pnpm and producing npm warnings.
4. `extractRuntimeImports()` treats `import(name)` as a package named `name` because it trims quotes from every dynamic import argument without checking whether the argument is a literal.
5. `IpcClient.dispose()` emits `disconnect`, so `DaemonManager.#cleanup()` can re-enter crash handling while already cleaning up a crashed client.
6. When imports are resolved but the daemon is unavailable, `DocumentAnalysisController` leaves states as `loading`, causing indefinite `...` inlay hints.
7. The Rust size pipeline is intentionally incomplete per `docs/ImportLens-SRS.md`: it does not satisfy FR-018 or FR-019. Do not make cosmetic estimator changes and claim release-grade tree-shaking.

## Task 1: Parser Regression

**Files:**
- Modify: `extension/test/imports/parser.test.ts`
- Modify: `extension/src/imports/parser.ts`

- [ ] **Step 1: Write the failing test**

Add a test that asserts `import(moduleName)` and interpolated template imports are ignored, while string-literal dynamic imports are still detected.

- [ ] **Step 2: Verify red**

Run: `pnpm test:ts`

Expected: the new parser test fails because `name` is currently detected.

- [ ] **Step 3: Implement literal-only dynamic import parsing**

Replace quote trimming with a helper that accepts only `'pkg'`, `"pkg"`, and template literals without `${...}`.

- [ ] **Step 4: Verify green**

Run: `pnpm test:ts`

Expected: all TypeScript tests pass.

- [ ] **Step 5: Commit**

Commit message: `fix: ignore non-literal dynamic imports`

## Task 2: IPC Lifecycle

**Files:**
- Create: `extension/test/ipc/client.test.ts`
- Modify: `extension/src/ipc/client.ts`
- Modify: `extension/src/daemon/manager.ts`

- [ ] **Step 1: Write the failing test**

Add a named-pipe/Unix-socket test proving explicit `IpcClient.dispose()` does not emit `disconnect`, and a server-close test proving an external close emits exactly one disconnect.

- [ ] **Step 2: Verify red**

Run: `pnpm test:ts`

Expected: dispose test fails because `disconnect` is emitted during intentional disposal.

- [ ] **Step 3: Implement idempotent close handling**

Track closed/disposed state in `IpcClient`, reject pending requests once, suppress disconnect on explicit dispose, and make `DaemonManager.#cleanup()` detach references before disposing.

- [ ] **Step 4: Verify green**

Run: `pnpm test:ts`

Expected: all TypeScript tests pass.

- [ ] **Step 5: Commit**

Commit message: `fix: make ipc client disposal non-reentrant`

## Task 3: Unavailable Daemon State

**Files:**
- Modify: `extension/src/analysis/state.ts`
- Modify: `extension/src/listener.ts`
- Modify: `extension/src/ui/decorations.ts`
- Modify: `extension/src/ui/inlayHints.ts`
- Modify: `extension/test/analysis/request.test.ts`

- [ ] **Step 1: Write the failing test**

Add a pure-state test for converting pending loading states to `unavailable` with a message while preserving missing and ready states.

- [ ] **Step 2: Verify red**

Run: `pnpm test:ts`

Expected: the helper does not exist.

- [ ] **Step 3: Implement unavailable state mapping**

Add the helper in `analysis/state.ts`, use it when `daemon.state !== "ready"` and in analysis catch blocks, and render `unavailable` in decorations/inlay hints.

- [ ] **Step 4: Verify green**

Run: `pnpm test:ts`

Expected: all TypeScript tests pass.

- [ ] **Step 5: Commit**

Commit message: `fix: surface unavailable daemon analysis state`

## Task 4: Target-Aware Build Scripts

**Files:**
- Create: `scripts/targets.mjs`
- Create: `scripts/targets.test.mjs`
- Create: `scripts/build-daemon.mjs`
- Create: `scripts/assert-vsix-size.mjs`
- Modify: `scripts/copy-daemon.mjs`
- Modify: `scripts/package-vsix.mjs`
- Modify: `package.json`
- Modify: `Cargo.toml`
- Modify: `Dockerfile.cross`
- Modify: `scripts/docker-build-entrypoint.sh`
- Modify: `compose.yaml`
- Add: `.dockerignore`

- [ ] **Step 1: Write failing script tests**

Cover VSIX target to Rust target mapping, binary names, exact Cargo artifact paths, package output names, and the absence of host fallback.

- [ ] **Step 2: Verify red**

Run: `node --test "scripts/**/*.test.mjs"`

Expected: test file fails because `scripts/targets.mjs` does not exist.

- [ ] **Step 3: Implement shared target metadata and exact copy**

Add `targets.mjs`, update `copy-daemon.mjs` to require `target/<rust-target>/release/<binary>` only, and add `build-daemon.mjs` that runs `cargo build --release --target <rust-target> -p import-lens-daemon` or `cargo zigbuild` when requested.

- [ ] **Step 4: Replace npm staging with pnpm**

Update `package-vsix.mjs` to use `pnpm install --prod --no-lockfile --config.node-linker=hoisted` inside staging and invoke `vsce` without `shell: true`.

- [ ] **Step 5: Fix Docker and Rust MSRV**

Set Rust MSRV to 1.89, use a matching Docker Rust image, run Docker packaging through target-aware scripts, and make Compose return the builder exit code.

- [ ] **Step 6: Verify script tests**

Run: `pnpm test:scripts`

Expected: all script tests pass.

- [ ] **Step 7: Verify package scripts**

Run: `pnpm package:win32-x64`

Expected: Windows VSIX builds and `pnpm assert:vsix-size import-lens-win32-x64-0.1.0.vsix` passes.

- [ ] **Step 8: Commit**

Commit message: `build: make packaging target-aware`

## Task 5: GitHub Actions Release Workflow

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Rewrite workflow around validation and package jobs**

Use a validation job for `pnpm install --frozen-lockfile`, `pnpm check`, `pnpm test`, and `cargo fmt --check`. Use a package matrix with fixed runner labels for the six platform VSIX targets and package each target with `pnpm package:<target>`.

- [ ] **Step 2: Add artifact and size gates**

Upload each VSIX artifact and run `pnpm assert:vsix-size` before upload. Validate the manual `version` input against `package.json`.

- [ ] **Step 3: Verify workflow YAML shape locally**

Run: `Get-Content -Raw .github/workflows/release.yml`

Expected: no YAML syntax obvious errors; all commands use pnpm.

- [ ] **Step 4: Commit**

Commit message: `ci: rebuild release workflow around target packages`

## Task 6: Final Verification

**Files:**
- All changed files

- [ ] **Step 1: Run required checks**

Run:

```powershell
pnpm check
pnpm test
cargo fmt --check
pnpm package:win32-x64
```

Expected: all exit 0.

- [ ] **Step 2: Run Docker verification**

Run: `pnpm docker:build`

Expected: exits non-zero on any builder failure; if cross-compilation succeeds, Linux/macOS VSIX files are produced and size-gated.

- [ ] **Step 3: Review diff and status**

Run: `git status --short` and `git diff --stat`.

Expected: only planned files changed, commits are focused, and generated hash changes correspond to the rebuilt daemon binary.
