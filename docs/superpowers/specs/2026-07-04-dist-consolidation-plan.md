# Consolidate build artifacts under a single `dist/`

## Goal

Replace the scattered top-level build-output directories with one organized `dist/`
folder. `target/` (Rust) stays where it is — it is the one universally-understood
convention, and moving it would force reconfiguring `rust-analyzer`, `cargo`, and the
CI `Swatinem/rust-cache`.

## Target layout

```
dist/
├── extension/extension.cjs   # bundled extension (tsdown outDir; isolated so clean:true is safe)
├── bin/<target>/<binary>     # per-target daemon binaries (shipped)
├── vsix/*.vsix               # packaged VSIXes
├── staging/<target>/         # vsix packaging scratch
└── test-dist/                # compiled extension tests
target/                        # Rust build output — UNCHANGED
```

### Path mapping

| Old | New |
| --- | --- |
| `extension/dist/extension.cjs` | `dist/extension/extension.cjs` |
| `extension/test-dist/` | `dist/test-dist/` |
| `bin/<target>/` | `dist/bin/<target>/` |
| `builds/` | `dist/vsix/` |
| `.vsix-staging/` | `dist/staging/` |
| `coverage/`, `wasm/` | dropped (stale `.gitignore` entries; coverage writes under `target/`) |
| `target/` | unchanged |

## Why `dist/extension/` and not `dist/`

`tsdown.config.ts` has `clean: true`. In packaging, `copy-daemon` writes `dist/bin/**`
*before* `pnpm build` runs. If the bundle's `outDir` were `dist/` root, that build would
wipe `dist/bin`. Keeping the bundle in `dist/extension/` scopes the clean.

## The runtime-contract constraint

`bin/<target>/…` is not just an output path — it is resolved at runtime and used as an
integrity-hash key, in **three** places that must move together:

1. `extension/src/daemon/nativeTransport.ts:148` — `bin/${target}/…`, used both to locate
   the binary (`extensionPath` + path) and as the hash-lookup key.
2. `scripts/daemon-hashes.mjs:53` `relativeDaemonPath` — generates the hash keys and reads
   the binary to hash.
3. `extension/src/daemon/knownHashes.generated.ts` — committed keys are literally
   `bin/<target>/…` (values are content hashes, unchanged by the move).

In dev (F5) `extensionPath` **is the repo root**, so the repo path and the in-VSIX path
must stay identical — we cannot decouple them. `main` (`extension/dist/extension.cjs`) is
likewise shared between dev and the packaged extension.

## Decoupling analysis

Two kinds of decoupling are possible here; only one is worth doing.

**Rejected — decouple repo layout from packaged/runtime layout.** e.g. build to `dist/bin`
but ship/resolve at `bin`. Impossible to do cleanly because dev resolves against the repo
root; the only workaround is a runtime env-var/dev-override for the daemon dir and bundle,
which adds permanent runtime complexity and a launch-config dependency for no real gain.
Having artifacts at `dist/…` in both dev and prod is fine. **Do not plan this.**

**Adopted — decouple the path *definitions* from their call sites.** `bin/<target>/<binary>`
is currently re-hardcoded in ~7 places across three shipping boundaries, which is exactly
what makes this move risky (miss one → silent daemon-resolution or hash-key drift). Route
each boundary through a single source:

- Extension: add `daemonRelativePath(target)` to `extension/src/daemon/platform.ts`;
  `nativeTransport.ts` uses it for both the file path and the hash-lookup key.
- Scripts: promote `relativeDaemonPath` from `daemon-hashes.mjs` into `targets.mjs` alongside
  new `dist/extension`, `dist/vsix`, `dist/staging` helpers; `copy-daemon`, `package-vsix`,
  `package-vsix-manifest`, and the hash generator all consume them.
- CLI (`cli/importlens.mjs`): stays self-contained (shipped standalone, cannot import build
  scripts) but centralized in one local constant.

This lands at 3 definitions — one per boundary, an inherent limit since the bundled
extension, the build scripts, and the standalone CLI cannot share a runtime module — guarded
by **one test** asserting the extension's computed path equals the generated hash keys. The
payoff: the actual `bin/ → dist/bin/` move becomes a one-line change per helper.

## Work plan (3 commits)

### Commit 1 — relocate no-contract build outputs (safe)
`builds/ → dist/vsix`, `.vsix-staging/ → dist/staging`, `extension/test-dist/ → dist/test-dist`.
These have no runtime contract, so they move directly.

- `tsconfig.test.json` outDir → `./dist/test-dist`; `package.json` `test:ts` (rm path + glob).
- `scripts/targets.mjs` `vsixNameForTarget` → `dist/vsix/…` (add `vsixDir`/`stagingDir` helpers).
- `scripts/assert-vsix-size.mjs` builds dir → `dist/vsix`; `scripts/package-vsix.mjs` staging → `dist/staging`.
- `scripts/docker-build-entrypoint.sh` — the four `builds/import-lens-…-${version}.vsix` size-gate
  args → `dist/vsix/…` (the local Docker build path breaks otherwise).
- GitHub Actions — all 7 old-path references (verified by grep; ci.yml/validate.yml have none,
  and release's "Locate build run" matches artifact *names*, not paths):
  - `build.yml:67` `VSIX_PATH: builds/…` → `dist/vsix/…` (cache restore/save and artifact upload
    all consume this one env var).
  - `release.yml:134` download `path: builds`, `:153` verify-script template, `:164` size gate,
    `:197` `gh release create builds/*.vsix`, `:206` + `:213` publish loops → `dist/vsix`.
- **Bump the VSIX cache-key namespace** in `build.yml` (`vsix-…` → `vsix-v2-…`, both restore and
  save). The key does not encode the path: without the bump, a same-version re-run restores the
  cached VSIX to the old `builds/` path, every build step skips on the hit, and the upload
  (`path: dist/vsix/…`, `if-no-files-found: error`) fails the job.
- Tests: `scripts/test/targets.test.mjs`; sweep the workflow tests for `builds` and the cache key.

Ordering note (verified safe): in `package-target.mjs`, `copy-daemon` writes `dist/bin` before
`pnpm build` runs tsdown — its `clean: true` wipes only its own `outDir` (`dist/extension`), so
sibling `dist/` content survives. Staging (`dist/staging/<target>`) copies only specific
subpaths, never `dist/` recursively, so no self-nesting occurs.

### Commit 2 — introduce single-source path helpers (pure refactor, NO move)
Route the duplicated `bin/<target>/…` and bundle definitions through one helper per boundary,
still pointing at the **current** `bin/` / `extension/dist/` locations. Behaviour-preserving;
all tests stay green.

- `scripts/targets.mjs`: promote `relativeDaemonPath` here; add `extensionBundlePath`.
  `daemon-hashes.mjs`, `copy-daemon.mjs`, `package-vsix.mjs`, `package-vsix-manifest.mjs` consume them.
- `extension/src/daemon/platform.ts`: add `daemonRelativePath(target)`; `nativeTransport.ts` uses it
  (path + hash key).
- `cli/importlens.mjs`: extract the daemon-path segment to one local constant.
- New test (cross-boundary contract guard) — MUST assert both directions:
  1. the extension's `daemonRelativePath(target)` equals the scripts' `relativeDaemonPath(target)`
     for every platform target, and
  2. **every key actually present in `extension/src/daemon/knownHashes.generated.ts`** parses as
     `relativeDaemonPath(t)` for a known target — so a stale or half-renamed committed key set
     fails the suite instead of silently disabling the daemon at runtime.

### Commit 3 — flip the helpers to `dist/` (the actual shipped move)
Now a one-line change per helper, plus regeneration.

- `tsdown.config.ts` + `tsconfig.json` outDir → `./dist/extension`; `package.json` `main` +
  `files` → `dist/extension/extension.cjs`, `dist/bin/`.
- Flip `targets.mjs` daemon/bundle helpers → `dist/bin/…`, `dist/extension/…`; flip
  `platform.ts` `daemonRelativePath` → `dist/bin/…`; flip the CLI constant.
- Rewrite `extension/src/daemon/knownHashes.generated.ts` by **mechanically renaming all six
  committed keys** `bin/<t>/… → dist/bin/<t>/…` (values are content hashes — unchanged).
  ⚠️ Do NOT do this by running `pnpm hash:daemon`: `updateKnownDaemonHashes` replaces only the
  *selected* targets' keys and keeps every other entry verbatim, so a local run would produce a
  mixed file (one new `dist/bin/…` key + five stale `bin/…` keys). The extension would then compute
  `dist/bin/…` on all platforms, miss the stale keys, fail integrity verification, and the daemon
  would be **silently unavailable on the five platforms not rebuilt locally**. The Commit 2
  contract test (key-set assertion) is what catches this class of mistake.
- Tests that hardcode the old paths and MUST be updated in this commit:
  - `scripts/test/bundle-externals.test.mjs:7` — reads `../../extension/dist/extension.cjs`.
  - `scripts/test/importlens-cli.test.mjs:120` — asserts the CLI daemon path contains
    `bin/win32-x64/import-lens-daemon.exe`.
  - `scripts/test/daemon-hashes.test.mjs` — fixture paths and expected hash keys use `bin/…`.

### Cross-cutting (folded into the commits above)
- `.gitignore`: add `/dist/` (**anchored** — unanchored `dist/` would also ignore any future
  nested `dist` fixture directory); drop `extension/dist/`, `extension/test-dist/`, `bin/`,
  `builds/`, `.vsix-staging/`, `coverage/`, `wasm/`. Keep `target/`, `node_modules/`, `*.vsix`,
  `*.log`, `.worktrees/`, `daemon/tests/fixtures/packages/`.
- `.dockerignore`: replace `.vsix-staging`, `bin`, `coverage`, `extension/dist`,
  `extension/test-dist`, `wasm` with `dist`. Keep `.git`, `node_modules`, `target`, `*.log`, `*.vsix`.
- Living docs that state the old paths (update):
  - `AGENTS.md:47-48`, `.agents/rules/instructions.md:48-49`
  - `.agents/skills/project-scaffolding/SKILL.md` (~67-73, 156, 194 — repo tree, `main`, gitignore note)
  - `.agents/skills/ci-cross-compilation/SKILL.md` (~42-43, 59 — executable `cp`/`sha256sum` commands
    targeting `bin/<platform>/`)
  - `.agents/skills/ts-daemon-lifecycle/SKILL.md` (~15, 34 — `bin/<platform>/` references)
  - `docs/ImportLens-SRS.md` (~214, ~664, ~736, ~1279, ~1723-1735 — binary location prose, bundler
    output path, locate-binary flow, repo tree)
- Historical records (do NOT update): dated files under `docs/superpowers/plans/` and
  `docs/superpowers/specs/` — they document past states of the repo.
- Final `git grep` sweep for `builds`, `\.vsix-staging`, `extension/dist`, `extension/test-dist`,
  `"bin/`, **and bare `bin/`** (filtering `#!/usr/bin/env`, `/usr/local/bin`, cargo `--bin`
  noise). Note the sweep misses `path.join(…, "bin", target)` / `join(…, "builds")` forms —
  those sites are already enumerated by filename above, but re-grep for `"bin"` and `"builds"`
  as quoted single segments to be certain.

### Audited and verified clean — no changes needed
- `compose.yaml` — mounts `.:/workspace` plus `node_modules`/`.pnpm-store` volumes only.
- `pnpm-workspace.yaml`, `.vscode/` (only `mcp.json`; no `launch.json`), no `.vscodeignore`
  (packaging uses the staged manifest `files` whitelist, so `dist/**` inclusion is explicit).
- `scripts/generate-daemon-hashes.mjs` — paths flow through `daemon-hashes.mjs`/`targets.mjs`
  helpers; its output file `extension/src/daemon/knownHashes.generated.ts` is source, not artifact.
- `scripts/test/package-vsix-manifest.test.mjs` — asserts only `LICENSE`/`cli/`; no path edits.
- `scripts/test/docker-compose-config.test.mjs`, `scripts/test/extension-configuration.test.mjs`
  — no artifact-path references.
- `extension/test/daemon/nativeTransport.test.ts` — fabricates no `bin/` fixture (binary
  verification is intentionally allowed to fail); re-verify it still passes after Commit 3.

### Do NOT touch (look-alikes that are not artifact paths)
- `package.json` `"bin"` field — the npm CLI entry (`./cli/importlens.mjs`), unrelated to `bin/`.
- `Dockerfile.build` `/usr/local/bin/zig` and `scripts/accuracy-compare.mjs` `--bin` (cargo flag).
- `daemon/Cargo.toml` `[[bin]]` section and `target:`/`rust-target:` keys in workflows/tsconfigs.

## Verification (before claiming done)
1. `pnpm test` green (script + TS + Rust suites), after each commit.
2. `pnpm build` → `dist/extension/extension.cjs` exists.
3. Local native package: `pnpm package:win32-x64` → a `dist/vsix/*.vsix`; unzip and confirm it
   contains `dist/extension/extension.cjs` and `dist/bin/win32-x64/import-lens-daemon.exe`.
4. **Launch the extension** (verify skill) and confirm the daemon spawns and its integrity check
   passes — this is the one path that can silently break from the hash-key move.
```
