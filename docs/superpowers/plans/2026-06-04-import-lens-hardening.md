# ImportLens Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix confirmed packaging and type-only package correctness gaps without changing behavior for false-positive weakness-list items.

**Architecture:** Keep the daemon protocol stable. Packaging hash generation becomes a small tested script helper plus a thin CLI wrapper. Declaration-only packages are detected in the daemon after normal entry resolution fails and are surfaced as successful zero-byte results with a structured diagnostic that existing UI can label.

**Tech Stack:** pnpm scripts, Node test runner, TypeScript 6, Rust daemon tests, existing MessagePack protocol v3.

---

## Confirmed Findings

- **Packaging hash overwrite bug:** `scripts/package-target.mjs` calls `scripts/generate-daemon-hashes.mjs <target>`, and `generate-daemon-hashes.mjs` writes a brand-new object from only the selected targets. A multi-target Docker/native package run therefore leaves `extension/src/daemon/knownHashes.generated.ts` containing only the last packaged target.
- **Declaration-only packages appear unavailable:** Packages such as `@types/*` or pure type helper packages can have `package.json` plus `.d.ts` files but no runtime JS entry. Current resolution reports an entry error and the UI shows `unavailable`, even though runtime cost is zero.
- **Shared dependency insights are partially implemented:** The daemon computes full internal contributions for shared byte math, but protocol serialization intentionally hides `internal_contributions`. The extension can name shared modules only when they are in public `module_breakdown`; otherwise it shows the SRS-required generic shared-byte message. This is valid but can be improved later with a protocol field for shared module summaries.

## Rejected False Positives

- **Astro `<script type="module">`:** Current Astro documentation says scripts are processed only when they have no attributes other than `src`; `type="module"` disables processing. The existing `isProcessedAstroScript()` behavior matches that rule.
- **Server + CJS suffix:** The SRS requires server runtime labeling and CJS warning indicators. `server · CJS` is not a confirmed bug.
- **Bundle text surgery:** `bundle.rs` is complex and should eventually move toward AST/codegen-based rewriting, but no failing input was confirmed in this pass. Do not refactor it without a specific regression test.

## File Responsibilities

- `scripts/daemon-hashes.mjs`: New testable helper for reading generated hash maps, hashing selected bins, merging with existing known hashes, and emitting deterministic TypeScript.
- `scripts/generate-daemon-hashes.mjs`: Thin CLI wrapper around the helper.
- `scripts/daemon-hashes.test.mjs`: Node tests for preserving unrelated target hashes and deterministic output.
- `scripts/package-target.mjs`: May keep passing the target; preservation in the helper fixes sequential package builds.
- `daemon/src/pipeline/analyze.rs`: Detect declaration-only packages after entry resolution failure and construct zero-byte success results.
- `daemon/tests/analyze.rs`: Regression tests for declaration-only packages and non-type packages that should still report entry errors.
- `extension/src/ui/format.ts`: Recognize the daemon diagnostic and add a `types only` suffix.
- `extension/src/ui/tooltip.ts`: Add a concise type-only note in hover details.
- `extension/test/ui/format.test.ts`: Regression test for the display label.
- `docs/ImportLens-SRS.md`: Document declaration-only package behavior as a supported edge case.

## Task 1: Preserve Generated Daemon Hashes

**Files:**
- Create: `scripts/daemon-hashes.mjs`
- Create: `scripts/daemon-hashes.test.mjs`
- Modify: `scripts/generate-daemon-hashes.mjs`
- Modify: `extension/src/daemon/knownHashes.generated.ts`

- [x] **Step 1: Write failing script tests**

Add tests that import the new helper and assert:

```javascript
const existingSource = `export const knownDaemonHashes: Readonly<Record<string, string>> = {
  "bin/darwin-arm64/import-lens-daemon": "old-darwin"
};\n`;
const next = updateKnownDaemonHashes({
  repoRoot,
  selectedTargets: ["win32-x64"],
  existingSource,
});
assert.equal(next.hashes["bin/darwin-arm64/import-lens-daemon"], "old-darwin");
assert.equal(next.hashes["bin/win32-x64/import-lens-daemon.exe"], expectedWinHash);
```

Run:

```powershell
pnpm test:scripts -- scripts/daemon-hashes.test.mjs
```

Expected before implementation: fails because `scripts/daemon-hashes.mjs` does not exist.

- [x] **Step 2: Implement the helper**

Implement arrow-function exports:

```javascript
export const parseKnownHashesSource = (source) => { ... };
export const collectDaemonHashes = ({ repoRoot, selectedTargets }) => { ... };
export const updateKnownDaemonHashes = ({ repoRoot, selectedTargets, existingSource }) => { ... };
export const knownHashesSource = (hashes) => `export const knownDaemonHashes...`;
```

Use `targetInfo()` for binary names and SHA-256 over existing `bin/<target>/<binary>` files. Preserve unrelated existing entries when a selected-target run updates only one platform.

- [x] **Step 3: Keep the CLI thin**

Change `generate-daemon-hashes.mjs` to read the existing generated file when present, call `updateKnownDaemonHashes()`, and write the returned source.

- [x] **Step 4: Verify and regenerate**

Run:

```powershell
pnpm test:scripts -- scripts/daemon-hashes.test.mjs
pnpm hash:daemon
```

Expected: tests pass and `knownHashes.generated.ts` contains hashes for all locally present daemon binaries while keeping deterministic sorted keys.

- [x] **Step 5: Commit**

```powershell
git add scripts/daemon-hashes.mjs scripts/daemon-hashes.test.mjs scripts/generate-daemon-hashes.mjs extension/src/daemon/knownHashes.generated.ts
git commit -m "fix: preserve daemon hashes across target builds"
```

## Task 2: Report Declaration-Only Packages as Zero Runtime Cost

**Files:**
- Create: `daemon/src/pipeline/types_only.rs`
- Modify: `daemon/src/pipeline/analyze.rs`
- Modify: `daemon/src/pipeline/mod.rs`
- Modify: `daemon/tests/analyze.rs`
- Modify: `extension/src/ui/format.ts`
- Modify: `extension/src/ui/tooltip.ts`
- Modify: `extension/test/ui/format.test.ts`
- Modify: `docs/ImportLens-SRS.md`

- [x] **Step 1: Write failing daemon regression tests**

Add a test that creates `node_modules/@types/demo/package.json` and `index.d.ts` with no runtime files, calls `analyze_import()`, and expects:

```rust
assert_eq!(result.error, None);
assert_eq!(result.raw_bytes, 0);
assert_eq!(result.brotli_bytes, 0);
assert!(!result.side_effects);
assert!(result.diagnostics.iter().any(|diagnostic| diagnostic.stage == "types_only"));
```

Add a companion test with no `.d.ts` and no runtime files that still expects `entry_resolution`.

Run:

```powershell
cargo test -p import-lens-daemon --test analyze declaration_only -- --nocapture
```

Expected before implementation: declaration-only test fails with an entry error.

- [x] **Step 2: Implement declaration-only detection**

When `resolve_import_package()` returns an `entry_resolution` error, call a focused `types_only` pipeline helper. That helper locates the package root with `find_package_root()`, walks the package directory with existing-style safety caps, skips nested `node_modules` and common generated directories, and returns type-only only when at least one declaration file exists and no runtime file exists.

Declaration files:

```rust
path.ends_with(".d.ts") || path.ends_with(".d.mts") || path.ends_with(".d.cts")
```

Runtime files:

```rust
js, mjs, cjs, jsx, ts, tsx, mts, cts
```

Do not count declaration files as runtime files.

- [x] **Step 3: Return an explicit zero-byte result**

Construct an `ImportResult` with all byte fields set to zero, `side_effects: false`, `truly_treeshakeable: true`, `is_cjs: false`, `error: None`, and a `types_only` diagnostic with package root details.

- [x] **Step 4: Add UI labeling**

In `format.ts`, add a helper that checks `result.diagnostics.some((diagnostic) => diagnostic.stage === "types_only")` and append ` · types only` before approximate/CJS logic. In `tooltip.ts`, add `Type-only package: yes` when the diagnostic is present.

- [x] **Step 5: Update SRS**

Add an edge-case requirement that declaration-only packages are reported as zero runtime bytes with a type-only diagnostic instead of `unavailable`.

- [x] **Step 6: Verify**

Run:

```powershell
cargo test -p import-lens-daemon --test analyze declaration_only -- --nocapture
pnpm test:ts -- extension/test/ui/format.test.ts
```

Expected: targeted daemon and TS tests pass.

- [x] **Step 7: Commit**

```powershell
git add daemon/src/pipeline/analyze.rs daemon/tests/analyze.rs extension/src/ui/format.ts extension/src/ui/tooltip.ts extension/test/ui/format.test.ts docs/ImportLens-SRS.md
git commit -m "feat: mark declaration-only packages as type-only"
```

## Task 3: Final Verification

**Files:**
- No planned production edits.

- [x] **Step 1: Run project checks**

Run:

```powershell
pnpm check
pnpm test:scripts
cargo fmt --check
cargo test -p import-lens-daemon --test analyze declaration_only -- --nocapture
pnpm test:ts
```

- [x] **Step 2: Package Windows x64 if daemon code changed**

Run:

```powershell
pnpm package:win32-x64
```

Expected: Windows daemon rebuilds, hash refreshes, extension bundles, and VSIX size check passes.

- [x] **Step 3: Commit generated Windows packaging updates if any**

If `knownHashes.generated.ts` changes after packaging, commit it with:

```powershell
git add extension/src/daemon/knownHashes.generated.ts
git commit -m "chore: refresh Windows daemon hash"
```

## Deferred Validated Backlog

- Add a protocol field such as `shared_modules?: ModuleContribution[]` so hover/report insights can name shared modules even when they are outside the top-10 public `module_breakdown`.
- Split `listener.ts` into orchestration and partial-response application helpers after adding tests around stale partials, missing packages, and history writes. This is a maintainability improvement, not a current correctness bug.
- Replace `bundle.rs` span-rewrite heuristics with AST/codegen-backed module stitching only after a concrete failing bundle fixture is captured.
- Prototype a workspace report treemap without adding runtime dependencies to the extension bundle, likely as a static SVG/HTML layout generated from `WorkspaceReportRow`.
- Add an opt-in ESM/CJS migration assistant in the extension host only if network access, caching, privacy, and settings are specified in the SRS first.
