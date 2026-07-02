# Release Workflow Revamp — Design

**Date:** 2026-07-03
**Status:** Approved (pending spec review)
**Scope:** Replace the single `.github/workflows/release.yml` with a cost-aware, reliable, incremental two-workflow system (build + release), and add Open VSX ("community store") publishing.

---

## 1. Problem

The current `release.yml`:

- Cross-compiles the Rust daemon for **macOS** (`x86_64-apple-darwin`, `aarch64-apple-darwin`) on an Ubuntu runner via Docker + `cargo-zigbuild`. This is the primary failure point — Darwin cross-linking against a faked SDK is fragile and hard to debug.
- Builds all 6 targets on **every** run with no reuse, so a single failed target forces a full, expensive rebuild of everything.
- Couples build and release into one workflow, so you cannot retry a release without rebuilding.
- Publishes only to the Visual Studio Marketplace. No Open VSX (community store).

## 2. Goals

1. **Reliable builds** — no cross-compiling Darwin on Linux. Build every target on its real OS with the native toolchain and native SDK.
2. **Cost-aware, incremental builds** — never rebuild a target that already succeeded for the same version. Only missing targets are built on a re-run.
3. **Force override** — a flag to rebuild every target regardless of what already exists.
4. **Separate release step** — release consumes already-built artifacts; it does not build.
5. **Release fails fast** if not all 6 target artifacts are present.
6. **Dry-run release** — validate the artifacts + publish configuration without mutating anything.
7. **Three publish destinations, conditional:**
   - GitHub draft release — **always**.
   - Visual Studio Marketplace — **only if `VSCE_PAT` is set**.
   - Open VSX (community store) — **only if `OVSX_PAT` is set**.
8. **Absolute-latest action versions**, pinned.

## 3. Non-goals

- Automated version bumping / changelog authoring (release notes are auto-generated from git history via `gh --generate-notes`, but the version input is still supplied manually).
- Tag-push triggering (kept as manual `workflow_dispatch` per decision).

Note: the Docker/zig cross-compilation machinery is **fully removed** in this change (see §7.2), since native per-OS builds make it dead weight.

---

## 4. Decisions (locked)

| Decision | Choice |
| --- | --- |
| Darwin/Linux build strategy | **Native runner per OS** (no Docker, no `cargo-zigbuild` on the release path) |
| Trigger | **Manual `workflow_dispatch`** with a `version` input, verified against `package.json` |
| Community store | **Open VSX**, published with `ovsx` |
| Publish destinations | **Explicitly selected per run** via boolean inputs (not auto-detected from secret presence) |
| Default selection | `release_github=true`, `publish_vscode=false`, `publish_openvsx=false` |
| Missing secret for a selected store | **Fail fast** in an up-front preflight, before any artifact work |
| Release handoff | **Artifacts from the latest `build.yml` run** (found via `gh run` by version), not cache |
| Skip-what-exists mechanism | **`actions/cache`** keyed `vsix-<target>-<version>` |
| GitHub release | Draft (never auto-published) |
| Changelog | **AI-generated** from commits since the last tag when `AI_API_KEY` is set; **git-cliff** deterministic fallback otherwise |
| AI provider | **OpenAI-compatible endpoint**, default **Groq** (`llama-3.3-70b-versatile`), swappable via `AI_BASE_URL` / `AI_MODEL` |

---

## 5. Architecture

Two decoupled workflows.

```
build.yml   (dispatch: version, force)
  validate ──> package [matrix, native per-OS, cache-gated] ──> (artifacts, 1-day retention)

release.yml (dispatch: version, dry_run)
  locate latest build run for <version> ──> download 6 artifacts ──> verify all present + size
     ──> draft GitHub release
     ──> [if VSCE_PAT] publish to VS Marketplace
     ──> [if OVSX_PAT] publish to Open VSX
```

### 5.1 Build matrix (native per OS)

| target | runner | build kind | package script |
| --- | --- | --- | --- |
| `win32-x64` | `windows-latest` | native | `package:win32-x64` |
| `win32-arm64` | `windows-latest` | cross-link (link-only, reliable) | `package:win32-arm64` |
| `linux-x64` | `ubuntu-24.04` | native | `package:linux-x64` |
| `linux-arm64` | `ubuntu-24.04-arm` | native (free ARM runner) | `package:linux-arm64` |
| `darwin-x64` | `macos-latest` | cross-compile, **native Apple SDK** | `package:darwin-x64` |
| `darwin-arm64` | `macos-latest` | native | `package:darwin-arm64` |

- Both Darwin arches run on a single `macos-latest` (Apple silicon) runner. `x86_64-apple-darwin` is built with the genuine Apple SDK present on the runner via `rustup target add x86_64-apple-darwin` — this is the fix for the macOS failures. Intel `macos-13` runners are intentionally avoided (deprecating, more expensive).
- Uses the **existing `package:<target>` scripts** (the non-`:zig` variants), which already build the daemon, copy it, hash it, and run `vsce package`.
- `strategy.fail-fast: false` — one target's failure does not cancel the others, so a re-run only needs to rebuild the failures.

### 5.2 Incremental caching (the cost saver)

Each matrix job:

1. **Restore** cache with key `vsix-<target>-<version>` into `builds/`.
2. If **hit** and `force != true`:
   - The `.vsix` is already present. **Skip the Rust build entirely.**
   - Still upload it as this run's artifact (so this run's artifact set accumulates to the full set).
3. If **miss** or `force == true`:
   - Install toolchain, build natively, produce the `.vsix`.
   - **Save** cache `vsix-<target>-<version>`.
   - Upload the `.vsix` as this run's artifact.

Consequences:
- **New version** → all keys miss → full rebuild automatically ("version not matched → build all").
- **Same version, re-run** → previously built targets restore instantly from cache; only missing targets compile ("version matched → skip existing").
- **`force: true`** → restore step is skipped → every target rebuilds.
- Because every run (hit or miss) uploads what it has, **the newest `build.yml` run for a version always holds the most complete artifact set.** A run that had 5/6 becomes 6/6 on the next re-run (5 from cache, 1 built).

Cache notes:
- Cache keys are immutable. Under `force`, the fresh build is uploaded as an **artifact** (release's source of truth), but the stale cache entry lingers until it evicts (7 days unused / 10 GB LRU). This is harmless because **release consumes artifacts, not cache.**
- Cache is branch-scoped; all release dispatches run on `main`, so scope is consistent.

### 5.3 Artifacts & retention

- Each target uploads `import-lens-<target>-<version>.vsix` as an artifact named `import-lens-<target>-<version>`.
- `retention-days: 1`. (GitHub's minimum is 1 day — sub-day / "1 hour" retention is not supported by the platform. 1 day comfortably covers a build-then-release flow.)
- `if-no-files-found: error`.
- `build.yml` sets `run-name: "Build v${{ inputs.version }}"` so `release.yml` can locate the correct run by version.

### 5.4 `validate` gate

Runs once in `build.yml` before the matrix, on `ubuntu-24.04`:
- Verify `package.json` version matches the `version` input.
- Verify the release icon exists.
- `pnpm check`, `pnpm test`, performance smoke test, Rust coverage.

This ensures a broken build is never cached. It is cheap relative to six native compiles.

### 5.5 `release.yml`

**Inputs:**

| input | type | default | meaning |
| --- | --- | --- | --- |
| `version` | string | (required) | version to release; must match `package.json` |
| `release_github` | boolean | `true` | create the draft GitHub release |
| `publish_vscode` | boolean | `false` | publish to the Visual Studio Marketplace (MS official store) |
| `publish_openvsx` | boolean | `false` | publish to Open VSX (community store) |
| `dry_run` | boolean | `false` | validate everything and report the plan without creating or publishing anything |

Publish destinations are chosen **explicitly per run** by these toggles — not inferred from which secrets happen to exist. A run may target GitHub only, GitHub + Open VSX, all three, etc.

**Steps (in order):**

1. **Preflight — fail fast, before any real work:**
   - Verify `version` matches `package.json`.
   - Verify at least one destination is selected (else fail — nothing to do).
   - For every **selected** store, verify its secret is present: `publish_vscode` requires `VSCE_PAT`; `publish_openvsx` requires `OVSX_PAT`. **If any selected store's secret is missing, abort immediately** with a clear error (e.g. `Open VSX selected but OVSX_PAT is not configured`). This happens *before* locating or downloading artifacts, so we never do expensive work only to fail on a missing credential.
2. Locate the newest `build.yml` run whose `run-name` matches `Build v<version>` (via `gh run list --workflow build.yml --json ...`, `GH_TOKEN` = default token with `actions: read`).
3. `gh run download <run-id>` — pull all `import-lens-*-<version>` artifacts into `builds/`.
4. **Verify all 6 target VSIXs present** (the existing `existsSync` check over the 6 targets). **Fail if any missing.**
5. Run `pnpm assert:vsix-size builds/*.vsix`.
6. **Generate the changelog** (`notes.md`) from commits since the previous release tag — see §5.6. Non-mutating, so it runs in dry-run too.
7. **If `dry_run`:** print the plan — "all 6 artifacts present, sizes OK; would create draft release `vX.Y.Z` with the following notes: …(prints `notes.md`)…; would publish to: [selected destinations]" — and exit **without creating or publishing anything**. (The preflight in step 1 already confirmed the selected stores' secrets, so a dry run also proves the credentials are in place and lets you preview the changelog.)
8. **If not `dry_run`:**
   - If `release_github`: `gh release create "v<version>" builds/*.vsix --draft --target "$GITHUB_SHA" --title "ImportLens <version>" --notes-file notes.md`.
   - If `publish_vscode`: for each `builds/*.vsix`, `pnpm exec vsce publish --packagePath "$file" --pat "$VSCE_PAT"`.
   - If `publish_openvsx`: for each `builds/*.vsix`, `pnpm exec ovsx publish "$file" --pat "$OVSX_PAT"`.
   - Unselected destinations emit an explicit "not selected, skipping" log line.

Permissions: `contents: write` (create release), `actions: read` (download cross-run artifacts).
Checkout for this job uses `fetch-depth: 0` so tags and full history are available for changelog generation.

### 5.6 Changelog generation

A single script — `scripts/generate-changelog.mjs` — owns the whole flow and writes `notes.md`. It follows the repo's existing `scripts/*.mjs` convention and is unit-testable.

**Shared commit collection (both paths):**
1. Find the previous release tag: `git describe --tags --abbrev=0 --match 'v*' HEAD` (drafts do not create tags, so `HEAD`'s nearest `v*` tag is the last *published* release). Empty on the first-ever release → diff from the root.
2. Range = `<prev>..HEAD` (or all history if no prev tag).
3. Collect commits once: `git log <range> --no-merges --pretty=…` → the single source of truth for "what's in this release."

**AI path — used only when `AI_API_KEY` is set:**
- POST the collected commits to `${AI_BASE_URL}/chat/completions` (OpenAI-compatible) with model `${AI_MODEL}`, a system prompt instructing a clean, categorized markdown changelog that summarizes *actual* changes (features / fixes / perf / docs / internal) and ignores noise.
- **Best-effort:** any failure — missing/invalid key at request time, non-2xx, network error, empty or obviously malformed response — logs a warning and **falls through to the deterministic path**. The AI never blocks or fails a release.

**Deterministic path (git-cliff) — used when `AI_API_KEY` is unset or the AI path failed:**
- Run **git-cliff** over the same range with a committed `cliff.toml`, grouping by conventional-commit prefixes (`feat:` / `fix:` / `docs:` / `perf:` / breaking `!`) — the repo's history already follows this convention. git-cliff is installed in CI (via `taiki-e/install-action@git-cliff`, a single binary) — no persistent dependency.

**Config (env / repo variables), with defaults baked into the workflow:**

| var | kind | default | purpose |
| --- | --- | --- | --- |
| `AI_API_KEY` | secret | (unset) | Presence enables the AI path. **Optional** — its absence is *not* a preflight failure (unlike store PATs); it simply selects the deterministic fallback. |
| `AI_BASE_URL` | repo variable | `https://api.groq.com/openai/v1` | OpenAI-compatible base URL; repoint to Cerebras / OpenRouter / Gemini-compat without code changes. |
| `AI_MODEL` | repo variable | `llama-3.3-70b-versatile` | Model id for the chosen endpoint. |

Both the AI-rendered and git-cliff-rendered output feed `gh release create --notes-file notes.md`. Because the release is a **draft**, the maintainer reviews (and can edit) the generated notes before publishing — the safety net for AI non-determinism.

---

## 6. Dependency & config changes

- **`package.json`:** add `ovsx` to `devDependencies`, pinned to its latest version. Keep `@vscode/vsce`.
- **Open VSX prerequisite (one-time, manual, outside CI):** the `importlens` namespace must be created and the publisher's `OVSX_PAT` generated at open-vsx.org. If you select `publish_openvsx` without `OVSX_PAT` configured, the run fails fast in preflight (§5.5); if you don't select it, it's simply not attempted.
- **Changelog config (optional):** `AI_API_KEY` secret (unset → git-cliff fallback), plus `AI_BASE_URL` / `AI_MODEL` repo variables with Groq defaults baked into the workflow (§5.6). git-cliff is installed in CI via `taiki-e/install-action`.
- **Action versions:** all actions pinned to their absolute-latest release, verified against the marketplace at implementation time. Baseline (repo's current standard): `actions/checkout`, `actions/setup-node`, `pnpm/action-setup`, `actions/upload-artifact`, `actions/download-artifact`, plus newly-introduced `actions/cache` and `taiki-e/install-action` (for git-cliff). The AI changelog uses a plain OpenAI-compatible HTTP call, not an action.

---

## 7. Files touched

### 7.1 Workflow & publishing

- **New:** `.github/workflows/build.yml`
- **Rewritten:** `.github/workflows/release.yml`
- **New:** `scripts/generate-changelog.mjs` — commit collection + AI/git-cliff rendering → `notes.md` (with a `scripts/test/generate-changelog.test.mjs` for the deterministic/range logic).
- **New:** `cliff.toml` — git-cliff config for the deterministic changelog.
- **Edited:** `package.json` — add `ovsx` devDependency (pinned). git-cliff itself is installed in CI (not an npm dependency).

### 7.2 Docker/zig removal (complete)

Native per-OS builds make the Docker + `cargo-zigbuild` cross-compilation path dead code. It is removed entirely.

**Deleted files:**
- `Dockerfile.build`
- `compose.yaml`
- `scripts/docker-build-entrypoint.sh`
- `scripts/test/docker-compose-config.test.mjs`

**Edited files:**
- `package.json` — remove scripts: `docker:build`, `package:linux-x64:zig`, `package:linux-arm64:zig`, `package:darwin-x64:zig`, `package:darwin-arm64:zig`, `package:all:zig`.
- `scripts/targets.mjs` — remove the `cargoZigbuildArgsForTarget` export.
- `scripts/build-daemon.mjs` — remove the `--zigbuild` flag handling and the `cargoZigbuildArgsForTarget` import (native `cargo build` only).
- `scripts/package-target.mjs` — remove the `--zigbuild` flag handling and update the usage string.
- `scripts/test/targets.test.mjs` — remove the `cargoZigbuildArgsForTarget` test and its import.
- `scripts/test/dependency-policy.test.mjs` — remove all `Dockerfile.build` / zig / `cargo-zigbuild` assertions (the `dockerfile` reads and the ZIG/CARGO_ZIGBUILD/zig-download matchers in both tests).

**Verification after removal:** `pnpm test` (specifically `test:scripts`) passes with no dangling references to zig/docker; `pnpm package:<target>` still builds each target natively.

---

## 8. Success criteria

1. Running `build.yml` for a fresh version compiles all 6 targets, each on its native OS; **no macOS cross-compile failures**.
2. Re-running `build.yml` for the same version rebuilds **only** the targets whose cache is missing; already-built targets are restored and re-uploaded in seconds.
3. `force: true` rebuilds all 6 regardless of cache.
4. `release.yml` publishes to exactly the destinations selected by the `release_github` / `publish_vscode` / `publish_openvsx` toggles — no more, no less.
5. `release.yml` **fails in preflight** (before downloading artifacts) if a selected store's PAT secret is missing, or if no destination is selected, or if `version` mismatches `package.json`.
6. `release.yml` **fails** when any of the 6 artifacts is missing.
7. `release.yml` with `dry_run: true` runs the full preflight (including secret checks for selected stores), generates and prints the changelog, and reports the plan, but creates/publishes nothing.
8. A default run (`release_github` only) produces just the GitHub draft release and touches no store.
9. With `AI_API_KEY` set, the draft release notes are AI-generated from commits since the previous tag; if the AI call fails for any reason, the release still succeeds with git-cliff-generated notes.
10. With `AI_API_KEY` unset, the draft release notes are generated deterministically by git-cliff — no preflight failure, no AI call attempted.
