# Compiler-Stack Coordination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Repository override:** per CLAUDE.md, tasks in this plan do NOT commit individually. §4.3 of the design mandates that the command, configuration, documentation, tests, and automation move atomically, so the whole plan lands as ONE commit at the end (Task 8). Run the narrow per-task checks as written; run the full gate before the single commit.

**Goal:** Replace the OXC-only dependency coordination with the coordinated Rolldown/OXC compiler-stack workflow from `docs/superpowers/specs/2026-07-10-bundler-redesign-design.md` §4 — exact pins, a feature-gated rolldown candidate dependency, `deps:update:compiler`, a generated graph fingerprint, locked Cargo entry points, drift tests, and the renamed upgrade skill — with zero production behavior change.

**Architecture:** A new `scripts/compiler-stack.config.mjs` is the single source of truth for the coordinated versions (rolldown 1.1.5, oxc 0.139.0, oxc_resolver 11.23.0). A shared `scripts/compiler-stack-fingerprint.mjs` computes the set of `oxc*`/`rolldown*` packages reachable from `rolldown` out of `cargo metadata --locked`, and that generated fingerprint (`scripts/compiler-stack.fingerprint.json`) is drift-checked by tests. `scripts/update-compiler-stack.mjs` resolves candidate stacks through a temporary Cargo manifest outside the repository (Cargo is the compatibility authority — no hand-written semver math) before editing any tracked file. `scripts/deps-update-safe.mjs` wraps general updates and restores/validates the recorded stack before reporting success.

**Tech Stack:** Node 24 ESM scripts (`node --test`), Cargo/`cargo metadata`, pnpm, lefthook-verified Conventional Commits.

## Global Constraints

- Coordinated stack versions (from the spec §4.1, current repo already ships the OXC half): `rolldown` **=1.1.5**, every direct OXC monorepo crate **=0.139.0**, `oxc_resolver` **=11.23.0**.
- Exact `=` requirements only for the coordinated stack. `^1.1`, `~1.1.5`, `~0.139.0`, `>=1.1.5` are prohibited (spec §4.1).
- `rolldown` is an **optional** dependency behind the non-default Cargo feature **`rolldown-candidate`**; the default build's dependency graph and behavior must not change (spec §4.1).
- Only a dependency-update command may rewrite `Cargo.lock`; every other build/test/benchmark/coverage/packaging/CI Cargo command passes `--locked` (spec §4.4). `cargo fmt` resolves no dependencies and is exempt.
- The extension manifest must have no direct `rolldown` or `oxc-parser` dependency in any dependency section (spec §4.5).
- `oxc_mangler` stays banned as a direct dependency.
- Tests derive every version from `scripts/compiler-stack.config.mjs` — never a literal (CLAUDE.md testing policy, spec §4.4).
- The old `deps:update:oxc` command and `oxc-stack*` file names are replaced, not aliased (spec §4.3).
- Files stay LF. pnpm only. No production behavior change anywhere in this plan.

## File Structure

| path | action | responsibility |
| --- | --- | --- |
| `daemon/Cargo.toml` | modify | exact pins; optional `rolldown`; `[features] rolldown-candidate` |
| `Cargo.lock` | regenerate | gains the rolldown subtree; existing crates must not move |
| `scripts/compiler-stack.config.mjs` | create | source of truth: versions + crate lists + feature name |
| `scripts/compiler-stack-fingerprint.mjs` | create | pure fingerprint derivation + `cargo metadata --locked` runner |
| `scripts/compiler-stack.fingerprint.json` | create (generated) | committed tuple set reachable from rolldown |
| `scripts/compiler-stack-helpers.mjs` | create (port of `oxc-stack-helpers.mjs`) | pure validation/rewrite helpers |
| `scripts/update-compiler-stack.mjs` | create (port of `update-oxc-stack.mjs`) | CLI: probe-resolve, validate, write, lockfiles, fingerprint |
| `scripts/deps-update-safe.mjs` | create | `pnpm update` + `cargo update` + stack restoration + fingerprint validation |
| `scripts/oxc-stack.config.mjs`, `scripts/oxc-stack-helpers.mjs`, `scripts/update-oxc-stack.mjs` | delete | replaced, not aliased |
| `scripts/test/update-compiler-stack.test.mjs` | create (port of `update-oxc-stack.test.mjs`) | updater logic tests |
| `scripts/test/compiler-stack-coordination.test.mjs` | create (port of `oxc-coordination.test.mjs`) | drift/guard tests incl. fingerprint |
| `scripts/test/update-oxc-stack.test.mjs`, `scripts/test/oxc-coordination.test.mjs` | delete | replaced |
| `scripts/targets.mjs` | modify | `--locked` in build/zigbuild/xwin arg builders |
| `scripts/check-coverage.mjs` | modify | `--locked` on `cargo llvm-cov` |
| `scripts/accuracy-compare.mjs` | modify | import/config rename only |
| `package.json` | modify | new script names; `--locked` on `test:rust`/`test:performance` |
| `.github/workflows/validate.yml` | modify | `--locked` on clippy |
| `.claude/skills/oxc-upgrade/` → `.claude/skills/compiler-stack-upgrade/` | rename + rewrite | covers rolldown + OXC + resolver workflow |
| `CLAUDE.md`, `docs/ImportLens-SRS.md` | modify | mechanical command/config-name references only |

---

### Task 1: Exact pins and the feature-gated rolldown dependency

**Files:**
- Modify: `daemon/Cargo.toml:17-27` (oxc pins), end of `[dependencies]`, new `[features]`
- Regenerate: `Cargo.lock`

**Interfaces:**
- Produces: `rolldown = { version = "=1.1.5", optional = true }` dependency line and `rolldown-candidate = ["dep:rolldown"]` feature that every later task's validation and fingerprint work depends on.

- [ ] **Step 1: Rewrite the coordinated pins in `daemon/Cargo.toml`**

Replace the ten `~0.139.0` oxc lines and `oxc_resolver = "~11.23.0"` with exact pins, add `rolldown` (alphabetical position after `redb`/before `rmp-serde` is NOT required — keep the dependency table alphabetical as it is today, so `rolldown` sits between `redb` and `rmp-serde`), and add the feature table at the end of the file:

```toml
oxc_allocator = "=0.139.0"
oxc_ast = "=0.139.0"
oxc_ast_visit = "=0.139.0"
oxc_codegen = "=0.139.0"
oxc_minifier = "=0.139.0"
oxc_parser = "=0.139.0"
oxc_resolver = "=11.23.0"
oxc_semantic = "=0.139.0"
oxc_span = "=0.139.0"
oxc_syntax = "=0.139.0"
oxc_transformer = "=0.139.0"
```

```toml
rolldown = { version = "=1.1.5", optional = true }
```

```toml
[features]
# Non-default qualification feature (spec §4.1): candidate daemon builds only.
rolldown-candidate = ["dep:rolldown"]
```

- [ ] **Step 2: Regenerate the lockfile minimally**

Run: `cargo metadata --manifest-path daemon/Cargo.toml --features rolldown-candidate --format-version 1 > /dev/null`
Expected: exit 0; `Cargo.lock` gains the rolldown subtree.

- [ ] **Step 3: Verify no existing crate moved**

Run: `git diff Cargo.lock | grep '^-' | grep -v '^---' | head`
Expected: no output except possibly the lockfile `version`/checksum shuffle for ADDED packages — any `-version = ...` line for an existing crate is a failure; investigate before continuing.

- [ ] **Step 4: Prove the default build is unchanged and the candidate compiles**

Run: `cargo check -p import-lens-daemon --locked`
Expected: clean (fast — warm cache, unchanged graph).

Run: `cargo check -p import-lens-daemon --locked --features rolldown-candidate`
Expected: clean (first run compiles the rolldown tree; minutes — run in background and continue with Task 2).

### Task 2: Config and fingerprint modules

**Files:**
- Create: `scripts/compiler-stack.config.mjs`
- Create: `scripts/compiler-stack-fingerprint.mjs`
- Create: `scripts/compiler-stack.fingerprint.json` (generated by Step 3, never hand-edited)

**Interfaces:**
- Produces: `compilerStackConfig` (`currentRolldownVersion`, `currentOxcVersion`, `currentResolverVersion`, `rolldownCrate`, `candidateFeature`, `oxcCrates: string[]`).
- Produces: `fingerprintFromMetadata(metadata, rootCrateName) -> { packages: [{name, version, source}] }` (pure), `computeCompilerStackFingerprint({ execFile, rootDir }) -> Promise<fingerprint>`, `FINGERPRINT_PATH`.

- [ ] **Step 1: Write `scripts/compiler-stack.config.mjs`**

```js
export const compilerStackConfig = {
  currentRolldownVersion: "1.1.5",
  currentOxcVersion: "0.139.0",
  currentResolverVersion: "11.23.0",
  rolldownCrate: "rolldown",
  candidateFeature: "rolldown-candidate",
  oxcCrates: [
    "oxc_allocator",
    "oxc_ast",
    "oxc_ast_visit",
    "oxc_codegen",
    "oxc_minifier",
    "oxc_parser",
    "oxc_semantic",
    "oxc_span",
    "oxc_syntax",
    "oxc_transformer",
  ],
};
```

- [ ] **Step 2: Write `scripts/compiler-stack-fingerprint.mjs`**

```js
import { execFile as execFileCallback } from "node:child_process";
import path from "node:path";
import { promisify } from "node:util";

export const FINGERPRINT_PATH = "scripts/compiler-stack.fingerprint.json";

// The fingerprint is the sorted (name, version, source) set of every oxc*/
// rolldown* package reachable from the top-level rolldown package in the
// feature-resolved, locked graph (spec §4.3). It is generated data.
export const fingerprintFromMetadata = (metadata, rootCrateName) => {
  const packagesById = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
  const nodesById = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  const root = metadata.packages.find((pkg) => pkg.name === rootCrateName);
  if (!root) {
    throw new Error(`${rootCrateName} is not present in the resolved graph`);
  }

  const seen = new Set();
  const queue = [root.id];
  const tuples = [];
  while (queue.length > 0) {
    const id = queue.shift();
    if (seen.has(id)) {
      continue;
    }
    seen.add(id);
    const pkg = packagesById.get(id);
    if (!pkg) {
      continue;
    }
    if (/^(?:oxc|rolldown)/u.test(pkg.name)) {
      tuples.push({ name: pkg.name, version: pkg.version, source: pkg.source ?? "path" });
    }
    for (const dep of nodesById.get(id)?.dependencies ?? []) {
      queue.push(dep);
    }
  }

  tuples.sort((left, right) =>
    left.name === right.name
      ? left.version.localeCompare(right.version)
      : left.name.localeCompare(right.name),
  );
  return { packages: tuples };
};

export const computeCompilerStackFingerprint = async ({
  execFile = promisify(execFileCallback),
  rootDir = process.cwd(),
  rootCrateName = "rolldown",
  candidateFeature = "rolldown-candidate",
} = {}) => {
  const { stdout } = await execFile(
    "cargo",
    [
      "metadata",
      "--locked",
      "--format-version",
      "1",
      "--manifest-path",
      path.join(rootDir, "daemon/Cargo.toml"),
      "--features",
      candidateFeature,
    ],
    { maxBuffer: 256 * 1024 * 1024 },
  );
  return fingerprintFromMetadata(JSON.parse(stdout), rootCrateName);
};

export const formatFingerprint = (fingerprint) => `${JSON.stringify(fingerprint, null, 2)}\n`;
```

Note: `metadata.resolve.nodes[].dependencies` is the flat id list; it exists in format-version 1 output. Do not use `deps[].dep_kinds` — dev-dependencies of non-workspace packages are never resolved, so the flat list is already the runtime graph for the rolldown subtree.

- [ ] **Step 3: Generate the committed fingerprint**

Run: `node -e "import('./scripts/compiler-stack-fingerprint.mjs').then(async (m) => process.stdout.write(m.formatFingerprint(await m.computeCompilerStackFingerprint()))) " > scripts/compiler-stack.fingerprint.json`
Expected: JSON containing `rolldown` at 1.1.5, `oxc_parser`/`oxc_resolver` at the configured versions, plus rolldown workspace crates (e.g. `rolldown_common`, `rolldown_resolver`) and separately-versioned oxc crates (e.g. `oxc_index`). Requires Task 1's lockfile.

- [ ] **Step 4: Spot-check the generated file**

Run: `node -e "const f = require('./scripts/compiler-stack.fingerprint.json'); console.log(f.packages.length, f.packages.filter(p => p.name === 'rolldown' || p.name === 'oxc_parser' || p.name === 'oxc_resolver').map(p => p.name + '@' + p.version).join(' '))" --input-type=commonjs`
Expected: a package count > 10 and `oxc_parser@0.139.0 oxc_resolver@11.23.0 rolldown@1.1.5` (order by name).

### Task 3: Helpers port — `compiler-stack-helpers.mjs`

**Files:**
- Create: `scripts/compiler-stack-helpers.mjs`

**Interfaces:**
- Consumes: `compilerStackConfig` (Task 2).
- Produces (used by Tasks 4, 5, 6): `validateCurrentStack(cargoToml)`, `validateVersion(label, version)`, `validateAvailableVersions(fetchJson, {rolldownVersion, oxcVersion, resolverVersion})`, `latestCrateVersion(fetchJson, crate)`, `updateCargoToml(cargoToml, {rolldownVersion, oxcVersion, resolverVersion})`, `updateManifest(manifestObject)`, `replaceKnownVersions(content, {rolldownVersion, oxcVersion, resolverVersion})`, `updateConfig(content, {rolldownVersion, oxcVersion, resolverVersion})`, `formatCompilerUpdateResult(result)`.

- [ ] **Step 1: Write the module (full content)**

Port `scripts/oxc-stack-helpers.mjs` with these behavioral changes, keeping its comment style:

```js
import { compilerStackConfig } from "./compiler-stack.config.mjs";

const semverPattern = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u;

const exactPin = (crate, cargoToml) => {
  const match = cargoToml.match(new RegExp(`^${crate}\\s*=\\s*"(=[^"]+)"$`, "mu"));
  return match?.[1].slice(1);
};

export const validateCurrentStack = (cargoToml) => {
  if (/^oxc_mangler\s*=/mu.test(cargoToml)) {
    throw new Error("oxc_mangler must not be present in daemon/Cargo.toml");
  }

  const crateVersions = compilerStackConfig.oxcCrates.map((crate) => {
    const version = exactPin(crate, cargoToml);
    if (!version) {
      throw new Error(`Missing exact pin (=) for OXC crate: ${crate}`);
    }
    return version;
  });
  const uniqueCrateVersions = new Set(crateVersions);
  if (uniqueCrateVersions.size !== 1) {
    throw new Error(
      `Current OXC crate versions are not coordinated: ${[...uniqueCrateVersions].join(", ")}`,
    );
  }

  if (!exactPin("oxc_resolver", cargoToml)) {
    throw new Error("Missing exact pin (=) for oxc_resolver");
  }

  if (!/^rolldown\s*=\s*\{\s*version\s*=\s*"=[^"]+",\s*optional\s*=\s*true\s*\}$/mu.test(cargoToml)) {
    throw new Error('Missing exact optional rolldown dependency (rolldown = { version = "=x.y.z", optional = true })');
  }
  if (!/^rolldown-candidate\s*=\s*\[\s*"dep:rolldown"\s*\]$/mu.test(cargoToml)) {
    throw new Error("Missing rolldown-candidate feature ([features] rolldown-candidate = [\"dep:rolldown\"])");
  }
};

export const validateVersion = (label, version) => {
  if (!semverPattern.test(version)) {
    throw new Error(`Invalid ${label} version: ${version}`);
  }
};

export const validateAvailableVersions = async (
  fetchJson,
  { rolldownVersion, oxcVersion, resolverVersion },
) => {
  await crateVersion(fetchJson, compilerStackConfig.rolldownCrate, rolldownVersion).catch(
    (error) => {
      throw new Error(`Unavailable rolldown version ${rolldownVersion}: ${error.message}`);
    },
  );

  await Promise.all(
    compilerStackConfig.oxcCrates.map((crate) =>
      crateVersion(fetchJson, crate, oxcVersion).catch((error) => {
        throw new Error(`Unavailable OXC crate ${crate}@${oxcVersion}: ${error.message}`);
      }),
    ),
  );

  await crateVersion(fetchJson, "oxc_resolver", resolverVersion).catch((error) => {
    throw new Error(`Unavailable oxc_resolver version ${resolverVersion}: ${error.message}`);
  });
};

export const latestCrateVersion = async (fetchJson, crate) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}`);
  const version = payload?.crate?.max_stable_version ?? payload?.crate?.newest_version;
  if (!version) {
    throw new Error(`Could not resolve latest crate version for ${crate}`);
  }
  return version;
};

export const updateCargoToml = (
  cargoToml,
  { rolldownVersion, oxcVersion, resolverVersion },
) => {
  let next = cargoToml;
  for (const crate of compilerStackConfig.oxcCrates) {
    next = next.replace(
      new RegExp(`^${crate}\\s*=\\s*"[^"]+"$`, "gmu"),
      `${crate} = "=${oxcVersion}"`,
    );
  }
  next = next.replace(
    /^oxc_resolver\s*=\s*"[^"]+"$/gmu,
    `oxc_resolver = "=${resolverVersion}"`,
  );
  return next.replace(
    /^rolldown\s*=\s*\{\s*version\s*=\s*"[^"]+",\s*optional\s*=\s*true\s*\}$/gmu,
    `rolldown = { version = "=${rolldownVersion}", optional = true }`,
  );
};

export const updateManifest = (manifest) => {
  const next = structuredClone(manifest);

  next.scripts = {
    ...(next.scripts ?? {}),
    "deps:update:compiler": "node scripts/update-compiler-stack.mjs",
    // General refresh stays range-respecting, but success now requires the
    // recorded compiler stack to survive it (spec §4.4) — hence a script, not
    // a bare `pnpm update && cargo update` chain.
    "deps:update:safe": "node scripts/deps-update-safe.mjs",
  };

  return `${JSON.stringify(next, null, 2)}\n`;
};

const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");

export const replaceKnownVersions = (
  content,
  { rolldownVersion, oxcVersion, resolverVersion },
) => {
  // Single word-boundary pass over all three pinned tokens, longest first, so
  // replacing one version can never corrupt another's needle.
  const replacements = new Map([
    [compilerStackConfig.currentRolldownVersion, rolldownVersion],
    [compilerStackConfig.currentOxcVersion, oxcVersion],
    [compilerStackConfig.currentResolverVersion, resolverVersion],
  ]);
  const needles = [...replacements.keys()]
    .sort((left, right) => right.length - left.length)
    .map(escapeRegExp);
  const pattern = new RegExp(`\\b(?:${needles.join("|")})\\b`, "gu");

  return content.replace(pattern, (match) => replacements.get(match) ?? match);
};

export const updateConfig = (content, { rolldownVersion, oxcVersion, resolverVersion }) =>
  content
    .replace(/currentRolldownVersion:\s*"[^"]+"/u, `currentRolldownVersion: "${rolldownVersion}"`)
    .replace(/currentOxcVersion:\s*"[^"]+"/u, `currentOxcVersion: "${oxcVersion}"`)
    .replace(/currentResolverVersion:\s*"[^"]+"/u, `currentResolverVersion: "${resolverVersion}"`);

export const formatCompilerUpdateResult = ({
  dryRun,
  rolldownVersion,
  oxcVersion,
  resolverVersion,
  changedFiles,
}) => {
  const mode = dryRun ? "Dry run" : "Updated";
  const files =
    changedFiles.length === 0 ? "No file edits needed." : `Files: ${changedFiles.join(", ")}`;
  return `${mode}: rolldown ${rolldownVersion}, OXC ${oxcVersion}, oxc_resolver ${resolverVersion}\n${files}\n`;
};

const crateVersion = async (fetchJson, crate, version) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}/${version}`);
  const returnedVersion = payload?.version?.num;
  if (returnedVersion !== version) {
    throw new Error(`crates.io returned ${returnedVersion ?? "no version"}`);
  }
};
```

### Task 4: Updater CLI — `update-compiler-stack.mjs`

**Files:**
- Create: `scripts/update-compiler-stack.mjs`
- Delete: `scripts/update-oxc-stack.mjs`, `scripts/oxc-stack-helpers.mjs`, `scripts/oxc-stack.config.mjs` (after Tasks 5-6 remove the last importers)

**Interfaces:**
- Consumes: Task 3 helpers, Task 2 config + fingerprint modules.
- Produces: `parseUpdateArgs(argv) -> { dryRun, rolldownVersion?, oxcVersion?, resolverVersion?, help? }` and `updateCompilerStack(options) -> Promise<{ rolldownVersion, oxcVersion, resolverVersion, changedFiles, dryRun }>` (test seam mirrors today's updater: injectable `fetchJson`, `readFile`, `writeFile`, `execFile`, `mkdtemp`, `rm`, `platform`, `stdout`).

Key behaviors (spec §4.3):

1. `--rolldown <v>` optional → defaults to `latestCrateVersion(fetchJson, "rolldown")`.
2. Probe resolution BEFORE any tracked-file edit: create a temp dir (`mkdtemp`), write `lib.rs` (empty) and a probe manifest:

```toml
[package]
name = "compiler-stack-probe"
version = "0.0.0"
edition = "2024"

[lib]
path = "lib.rs"

[dependencies]
rolldown = "=<requested>"
# plus, only when the caller passed explicit overrides:
oxc_parser = "=<oxc override>"     # one line per configured oxc crate
oxc_resolver = "=<resolver override>"
```

3. Run `cargo metadata --format-version 1 --manifest-path <tmp>/Cargo.toml` via the injected `execFile`. A non-zero exit = unsatisfiable stack → throw `Unsatisfiable compiler stack: …` with cargo's stderr; no tracked file has been touched. Always `rm` the temp dir (finally block).
4. Derive versions from the probe metadata: `resolvedVersion(metadata, "oxc_parser")` and `resolvedVersion(metadata, "oxc_resolver")` (`metadata.packages.find((pkg) => pkg.name === name)?.version`, throw if absent). When the caller passed `--oxc`/`--resolver`, assert the derived version equals the override (they were constraints in the probe, so a mismatch is a cargo bug — still assert).
5. `validateVersion` all three; `validateAvailableVersions` (existence of every configured oxc crate at the derived monorepo version — the umbrella `oxc` crate does not depend on all ten, so cargo's probe alone does not prove each exists).
6. `validateCurrentStack` on the real `daemon/Cargo.toml`, then rewrite: cargoToml/manifest/srs/config via Task 3 helpers. `changedFiles` reporting identical to today.
7. Non-dry: write changed files, then lockfiles: `pnpm install --lockfile-only` (shell:true on win32 — port the existing platform split verbatim), `cargo update -p rolldown --precise <v>`, `cargo update -p oxc_resolver --precise <v>`, one `cargo update -p <crate> --precise <v>` per configured oxc crate; then recompute the fingerprint via `computeCompilerStackFingerprint({ execFile })` and write `FINGERPRINT_PATH`.
8. Dry-run: probe + validations run; zero `writeFile` calls, zero lockfile/fingerprint `execFile` calls beyond the probe `cargo metadata`.
9. CLI surface: `--rolldown`, `--oxc`, `--resolver`, `--dry-run`, `-h/--help`, tolerate a bare `--` (port `parseUpdateArgs` and extend with `--rolldown`); default paths gain `fingerprint: "scripts/compiler-stack.fingerprint.json"` and `config: "scripts/compiler-stack.config.mjs"`.

- [ ] **Step 1: Write the module** (port `update-oxc-stack.mjs` structure: `defaultPaths`, `readFiles`, `readText`, `valueAfter`, `updateLockfiles`, `defaultFetchJson`, `printHelp`, main guard — with the behaviors above; help text documents `pnpm deps:update:compiler --rolldown <version> [--oxc <version>] [--resolver <version>] [--dry-run]`).
- [ ] **Step 2: Sanity-run help and dry-run against the live repo**

Run: `node scripts/update-compiler-stack.mjs --help`
Expected: usage text with all four options.

Run: `node scripts/update-compiler-stack.mjs --rolldown 1.1.5 --dry-run`
Expected: `Dry run: rolldown 1.1.5, OXC 0.139.0, oxc_resolver 11.23.0` and `No file edits needed.` (network + cargo required; the probe resolves in a temp dir).

### Task 5: Safe update — `deps-update-safe.mjs`

**Files:**
- Create: `scripts/deps-update-safe.mjs`

**Interfaces:**
- Consumes: Task 2 config + fingerprint modules.
- Produces: `runSafeUpdate({ execFile, platform, rootDir, readFile, stdout }) -> Promise<void>` (throws on any restoration/validation failure) plus a CLI main guard.

Behavior (spec §4.4):

1. `pnpm update` (shell:true on win32, args form elsewhere — same split as the updater), then `cargo update`.
2. Restore the recorded stack: `cargo update -p <name> --precise <configured version>` for `rolldown`, every `oxcCrates` entry, and `oxc_resolver`. A no-op precise pin succeeds; let real failures propagate.
3. Recompute the fingerprint via `computeCompilerStackFingerprint` and deep-compare (`JSON.stringify` equality of the parsed committed file vs the recomputed object) against `scripts/compiler-stack.fingerprint.json`. On mismatch, throw `deps:update:safe could not restore the compiler stack; run pnpm deps:update:compiler …` — the command must FAIL rather than present the update as safe. Restoration only re-pins the coordinated packages; a moved rolldown workspace crate (allowed by rolldown's caret ranges) is exactly what this comparison catches.
4. On success print a one-line summary to stdout.

- [ ] **Step 1: Write the module.**
- [ ] **Step 2: Do NOT run it against the live repo** (it would move unrelated dependencies — that is its job, but not this plan's). Its logic is covered by tests in Task 6.

### Task 6: Test suites port and extension

**Files:**
- Create: `scripts/test/update-compiler-stack.test.mjs`
- Create: `scripts/test/compiler-stack-coordination.test.mjs`
- Delete: `scripts/test/update-oxc-stack.test.mjs`, `scripts/test/oxc-coordination.test.mjs`
- Modify: `scripts/accuracy-compare.mjs` (rename its `oxc-stack.config.mjs` import to `compiler-stack.config.mjs` / `compilerStackConfig`)

**Interfaces:**
- Consumes: everything above. Versions always derived from `compilerStackConfig`.

`update-compiler-stack.test.mjs` — port every existing updater test to the new names and extend:

1. Fixtures: `cargoTomlFixture()` gains the rolldown line and feature table; `configFixture()` gains `currentRolldownVersion`; probe support = `probeMetadata({ rolldown, oxc, resolver })` returning a minimal `cargo metadata` JSON:

```js
const probeMetadata = ({ rolldown, oxc, resolver }) => ({
  packages: [
    { id: "id:rolldown", name: "rolldown", version: rolldown, source: "registry+crates-io" },
    { id: "id:oxc_parser", name: "oxc_parser", version: oxc, source: "registry+crates-io" },
    { id: "id:oxc_resolver", name: "oxc_resolver", version: resolver, source: "registry+crates-io" },
  ],
  resolve: {
    nodes: [
      { id: "id:rolldown", dependencies: ["id:oxc_parser", "id:oxc_resolver"] },
      { id: "id:oxc_parser", dependencies: [] },
      { id: "id:oxc_resolver", dependencies: [] },
    ],
  },
});
```

   The injected `execFile` mock answers `cargo metadata` (probe AND fingerprint recompute) with `{ stdout: JSON.stringify(probeMetadata(...)) }`, records every other invocation, and the injected `mkdtemp`/`rm` record temp lifecycle.
2. Ported cases (same assertions, new pins): args parsing (now incl. `--rolldown`), bare `--`, unknown option, dry-run writes nothing (probe metadata exec allowed, no writes, no lockfile execs), full update writes all files + runs `pnpm install --lockfile-only` + precise `cargo update` for rolldown/resolver/each oxc crate + writes the fingerprint file, win32 shell split, latest-resolution (rolldown latest from crates.io payload), invalid/unavailable version rejection before edits, non-coordinated pins rejection, mangler rejection.
3. New cases: (a) unsatisfiable probe — `execFile` mock rejects for the probe `cargo metadata` → rejects with `/Unsatisfiable compiler stack/` and zero `writeFile` calls; (b) missing rolldown optional line in the fixture → `/Missing exact optional rolldown dependency/`; (c) derived-vs-override mismatch asserts; (d) temp dir is removed even when the probe fails (the `rm` mock was called).
4. `runSafeUpdate` cases: happy path (fingerprint matches → resolves, ran pnpm update, cargo update, then one precise pin per coordinated package), mismatch path (recomputed fingerprint differs from committed → rejects with `/could not restore the compiler stack/`).

`compiler-stack-coordination.test.mjs` — port the four existing checks and extend (all versions from config; comment block explaining the compiler stack is the ONLY version any test may assert, per CLAUDE.md):

```js
test("every oxc monorepo crate is exact-pinned at the configured version", ...);   // `= "=0.139.0"`
test("oxc_resolver is exact-pinned at its own configured version", ...);
test("rolldown is an exact-pinned optional dependency behind rolldown-candidate", ...);
// asserts BOTH the dependency line and the `rolldown-candidate = ["dep:rolldown"]` feature line
test("oxc_mangler stays out of the dependency graph", ...);                        // unchanged guard
test("the oxc napi package stays out of the extension host", ...);                 // unchanged guard
test("rolldown never becomes a direct extension dependency", ...);
// Guard (spec §4.5): package.json dependencies/devDependencies have no "rolldown";
// transitive rolldown via tsdown stays permitted (pnpm-lock is not asserted).
test("the committed fingerprint matches the locked cargo graph", ...);
// Drift (spec §4.4): computeCompilerStackFingerprint() with the real cargo,
// deep-equal against scripts/compiler-stack.fingerprint.json. Requires cargo on
// PATH — already true wherever test:rust runs.
test("coordinated crates resolve to exactly one version each", ...);
// From the recomputed fingerprint: for rolldown, oxc_resolver, and every
// configured oxc crate present, assert exactly one distinct version.
```

- [ ] **Step 1: Write both test files with the full ported + new cases.**
- [ ] **Step 2: Delete the two old test files and the three old script files; update `scripts/accuracy-compare.mjs` imports.**
- [ ] **Step 3: Run the suite**

Run: `pnpm test:scripts`
Expected: PASS, including one real `cargo metadata --locked` execution (~1-2 s).

Run: `grep -rn "oxc-stack\|oxcStackConfig\|deps:update:oxc" scripts/ package.json extension/ daemon/ .github/`
Expected: no matches (docs and the skill are handled by Task 7; the frozen spec/plan history under `docs/superpowers/` may keep historical mentions).

### Task 7: Locked entry points, skill rename, doc references

**Files:**
- Modify: `package.json` (`test:rust`, `test:performance` — the script-name changes landed via Task 4's `updateManifest` shape; apply them here by hand since the updater is not run against the live repo in this plan)
- Modify: `scripts/targets.mjs:90-124` (three arg builders), `scripts/check-coverage.mjs:40`, `.github/workflows/validate.yml:102`
- Rename: `.claude/skills/oxc-upgrade/` → `.claude/skills/compiler-stack-upgrade/` (git mv; rewrite `SKILL.md` frontmatter + workflow, update `references/sources-and-surface.md`)
- Modify: `CLAUDE.md` (testing-policy example path + oxc-only wording), `docs/ImportLens-SRS.md` (mechanical `deps:update:oxc`/`oxc-stack.config.mjs` reference renames ONLY — the architecture rewrite is the separate post-approval SRS change per spec Phase 0)

- [ ] **Step 1: `--locked` sweep**

```json
"test:rust": "cargo test --workspace --locked",
"test:performance": "cargo test -p import-lens-daemon --release --locked --test performance -- --ignored --nocapture",
```

`targets.mjs`: `["build", "-p", "import-lens-daemon", "--release", "--locked", "--target", info.rustTarget]` and the same insertion for `zigbuild` and `xwin` arg arrays. `check-coverage.mjs`: `run("cargo", ["llvm-cov", "--workspace", "--locked", "--fail-under-lines", "70"]);`. `validate.yml`: `run: cargo clippy --workspace --all-targets --locked`. `cargo fmt --check` stays unlocked (no dependency resolution).

- [ ] **Step 2: Package script renames** — `deps:update:oxc` → `deps:update:compiler` (`node scripts/update-compiler-stack.mjs`), `deps:update:safe` → `node scripts/deps-update-safe.mjs`.

- [ ] **Step 3: Skill rename and rewrite**

`git mv .claude/skills/oxc-upgrade .claude/skills/compiler-stack-upgrade`, then in `SKILL.md`:
- frontmatter `name: compiler-stack-upgrade`; description extended to "Upgrade the coordinated compiler stack (Rolldown + the OXC monorepo crates + oxc_resolver) the RIGHT way…" and trigger phrases gain "update/upgrade rolldown", "bump the compiler stack".
- "What we depend on" section: three independently-versioned lines (rolldown exact-pinned, coordinated `=x.y.z` OXC monorepo crates, `oxc_resolver`), canonical list in `scripts/compiler-stack.config.mjs`; keep the five surface facts verbatim.
- Workflow step 1 becomes: read all three current versions from `scripts/compiler-stack.config.mjs`; establish the rolldown range FIRST (rolldown release notes/changelog current→target), because rolldown's caret requirements decide which OXC/resolver versions are even reachable; then the existing OXC/resolver changelog review for the versions Cargo derives.
- Command references: `pnpm deps:update:compiler --rolldown <v> [--oxc <v>] [--resolver <v>] [--dry-run]`; note that omitted `--oxc`/`--resolver` are derived by Cargo resolution, and that every post-adoption rolldown upgrade reruns the spec §10 qualification gates before shipping.
- `references/sources-and-surface.md`: add the rolldown release sources (github.com/rolldown/rolldown releases + CHANGELOG) alongside the existing OXC sources; update any `oxc-stack.config.mjs`/`deps:update:oxc` mention.

- [ ] **Step 4: Doc reference sweep** — CLAUDE.md testing-policy example reads `compiler-stack.config.mjs`; its "except oxc coordination" sentence becomes "except compiler-stack coordination (rolldown + OXC + oxc_resolver)"; SRS mentions of the old command/config get the new names verbatim, nothing else.

- [ ] **Step 5: Re-run the reference sweep from Task 6 Step 3 including docs**

Run: `grep -rn "deps:update:oxc\|oxc-stack" --include="*.md" --include="*.mjs" --include="*.json" --include="*.yml" . | grep -v node_modules | grep -v docs/superpowers`
Expected: no matches.

### Task 8: Full gate, single atomic commit, independent review

- [ ] **Step 1: Full verification**

Run: `pnpm check && pnpm test && cargo fmt --check`
Expected: all green (`pnpm test` = build + ts + scripts + rust; rust now runs `--locked`).

Run: `cargo check -p import-lens-daemon --locked --features rolldown-candidate`
Expected: clean (from Task 1's warm cache).

- [ ] **Step 2: Stage everything and review the diff**

Run: `git add -A && git status --short && git diff --cached --stat`
Expected: exactly the files in the File Structure table (plus `Cargo.lock`, the fingerprint, and the skill rename pair).

- [ ] **Step 3: Independent review (hybrid-execution)** — dispatch a read-only reviewer subagent over `git diff --cached` + this plan; verify each finding against the code; fix confirmed ones; decline the rest with one-line reasons.

- [ ] **Step 4: Single commit**

```bash
git commit  # type: build(deps), subject ~"adopt the coordinated compiler-stack workflow"
```

Body must cover: exact pins, optional rolldown 1.1.5 behind rolldown-candidate (default build unchanged), deps:update:compiler with Cargo-resolved compatibility, generated fingerprint + drift tests, --locked entry points, deps:update:safe restoration, skill rename. Conventional Commits, body required.

## Verification summary

| check | command |
| --- | --- |
| updater + coordination tests | `pnpm test:scripts` |
| default graph unchanged | `cargo check -p import-lens-daemon --locked` |
| candidate compiles | `cargo check -p import-lens-daemon --locked --features rolldown-candidate` |
| full gate | `pnpm check && pnpm test && cargo fmt --check` |
| no stale names | Task 7 Step 5 grep |

## Out of scope (next plan)

The Rust candidate engine — adapter, native plugin, virtual entry, construct matrix, measurement harness (spec §5-§10) — is planned separately once this plan lands, against the actual rolldown 1.1.5 sources fetched by Task 1.
