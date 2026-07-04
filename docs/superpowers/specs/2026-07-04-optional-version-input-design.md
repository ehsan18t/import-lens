# Optional, override-capable release version input

## Problem

The **Build** and **Release** workflows both take a `version` input that is `required: true` and
is validated to *equal* `package.json`'s `version` — the build fails fast on any mismatch. That is
redundant friction: the version already lives in `package.json`, so a release cannot proceed without
re-typing it, and the only thing the input can legally contain is the value already committed.

## Goal

Make the `version` input **optional** in both workflows:

- **Empty** → the effective version is read from `package.json` (the common case; no typing).
- **Non-empty** → that value is **authoritative**: the produced artifacts carry it, overriding
  `package.json` for that run.

## Non-goals / decisions

- **The override touches `package.json` only, never `daemon/Cargo.toml`.** The daemon's
  `CARGO_PKG_VERSION` is baked into `ANALYZER_VERSION` (`daemon/src/cache/key.rs`), which is a disk
  cache key. Bumping it would invalidate every user's cache on each overridden release. The VSIX
  version is read solely from `package.json` (via `vsce` and `vsixNameForTarget`), so a
  `package.json`-only write fully satisfies the goal without cache churn.
- **`concurrency.group` and `run-name` keep referencing the raw `inputs.version`.** GitHub evaluates
  these at workflow-start, before any step runs, with no filesystem access — `package.json` is
  unreadable there. A blank input collapses `build-${{ inputs.version }}` to `build-`; two
  version-less builds would serialize rather than run in parallel. Harmless.

## Design

### Scripts (each with a `scripts/test/*.test.mjs`)

- **`scripts/resolve-version.mjs [version]`** — read-only. Prints the trimmed argument if non-empty,
  otherwise `package.json`'s `version`. Single source of the "input || package.json" rule, shared by
  both workflows.
- **`scripts/set-version.mjs <version>`** — writes `version` into `package.json`, preserving key
  order and trailing newline, and **only rewriting when the value actually changed** (idempotent — an
  empty-input build that resolves to the current version performs no write). Fails on a missing or
  empty argument.

### `build.yml`

- `version` input → `required: false`.
- `run-name: Build ${{ inputs.version || 'from package.json' }}` (cosmetic).
- `validate` job: drop the "Verify release version" equality step.
- `package` job:
  1. After checkout, a **Resolve version** step runs `resolve-version.mjs`, writes the effective
     version to `$GITHUB_OUTPUT`, and derives `VSIX_PATH` into `$GITHUB_ENV`.
  2. Cache key, artifact name, and upload path reference the resolved version, not `inputs.version`.
  3. Before packaging, a **Set version** step runs `set-version.mjs <resolved>` so `vsce` bakes the
     right version into the VSIX and its filename.

### `release.yml`

- `version` input → `required: false`.
- A **Resolve version** step runs `resolve-version.mjs` and exports `RELEASE_VERSION` /
  `RELEASE_TAG` (replacing the job-level `env` derivation).
- **Preflight** drops the `package.json` equality check (release only downloads + publishes prebuilt
  VSIXs; the artifacts are the source of truth, and "Verify expected VSIX artifacts" already guards
  completeness).
- **Locate build run** stops matching on `run-name` (which can no longer carry the version when the
  input is blank). Instead it queries the Actions **artifacts API** for the version-stamped artifact
  `import-lens-win32-x64-<version>` and reads its `workflow_run.id`. More precise than the old name
  match.

### Tests & docs to update

- `scripts/test/build-workflow.test.mjs` — the cache-key assertion changes from `inputs.version` to
  the resolved value.
- `scripts/test/release-workflow.test.mjs` — adjust any preflight/locate assertions.
- `docs/release-checklist.md`, `docs/release-setup-guide.md` — reflect that the version is optional
  and that a typed value overrides `package.json`.
