import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { compilerStackConfig } from "../compiler-stack.config.mjs";
import {
  computeCompilerStackFingerprint,
  FINGERPRINT_PATH,
  formatFingerprint,
} from "../compiler-stack-fingerprint.mjs";

// Drift checks. daemon/Cargo.toml, compiler-stack.config.mjs, and the
// generated fingerprint are separately maintained records of the same
// coordinated stack; `pnpm deps:update:compiler` writes all of them. These
// fail when only one of them moved -- a real bug, caught before it ships.
//
// The compiler stack (rolldown + the OXC monorepo crates + oxc_resolver) is
// the ONLY dependency whose versions any test may assert. It is the only place
// where a bump can silently change analysis output. See the Testing Policy in
// CLAUDE.md before adding anything here.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const escapeVersion = (version) => version.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");

// One real `cargo metadata --locked` run shared by the fingerprint tests.
// Requires cargo on PATH -- already true wherever test:rust runs.
const recomputedFingerprint = await computeCompilerStackFingerprint({
  rootDir: fileURLToPath(new URL("../../", import.meta.url)),
});

test("every oxc monorepo crate is exact-pinned at the configured version", () => {
  const cargoToml = repoFile("daemon/Cargo.toml");

  for (const crate of compilerStackConfig.oxcCrates) {
    assert.match(
      cargoToml,
      new RegExp(`^${crate} = "=${escapeVersion(compilerStackConfig.currentOxcVersion)}"$`, "mu"),
      `${crate} must be pinned to =${compilerStackConfig.currentOxcVersion}`,
    );
  }
});

test("oxc_resolver is exact-pinned at its own configured version", () => {
  // Versioned independently of the monorepo crates, in a separate repository.
  assert.match(
    repoFile("daemon/Cargo.toml"),
    new RegExp(
      `^oxc_resolver = "=${escapeVersion(compilerStackConfig.currentResolverVersion)}"$`,
      "mu",
    ),
  );
});

test("rolldown is an exact-pinned optional dependency behind rolldown-candidate", () => {
  const cargoToml = repoFile("daemon/Cargo.toml");

  assert.match(
    cargoToml,
    new RegExp(
      `^rolldown = \\{ version = "=${escapeVersion(compilerStackConfig.currentRolldownVersion)}", optional = true \\}$`,
      "mu",
    ),
  );
  // The candidate feature must stay non-default: the shipped binary never
  // links rolldown.
  assert.match(cargoToml, /^rolldown-candidate = \["dep:rolldown"\]$/mu);
  assert.doesNotMatch(cargoToml, /^default = .*rolldown/mu);
});

test("oxc_mangler stays out of the dependency graph", () => {
  // Guard: mangling would change emitted identifiers and break size accuracy.
  assert.doesNotMatch(repoFile("daemon/Cargo.toml"), /^oxc_mangler = /mu);
});

test("the oxc napi package stays out of the extension host", () => {
  // Guard: analysis belongs to the Rust daemon. A JS oxc-parser in the host
  // would ship a second, independently versioned parser.
  const manifest = JSON.parse(repoFile("package.json"));

  assert.equal(manifest.dependencies["oxc-parser"], undefined);
  assert.equal(manifest.devDependencies["oxc-parser"], undefined);
});

test("rolldown never becomes a direct extension dependency", () => {
  // Guard (spec §4.5): the TypeScript build may use rolldown only transitively
  // through tsdown; a direct dependency would couple the extension host to the
  // Rust compiler stack's bundler.
  const manifest = JSON.parse(repoFile("package.json"));

  assert.equal(manifest.dependencies.rolldown, undefined);
  assert.equal(manifest.devDependencies.rolldown, undefined);
});

test("the committed fingerprint matches the locked cargo graph", () => {
  // Drift: `cargo update` moving any rolldown workspace crate or OXC package
  // reachable from rolldown -- allowed by rolldown's caret ranges without any
  // direct-pin change -- lands here.
  assert.equal(formatFingerprint(recomputedFingerprint), repoFile(FINGERPRINT_PATH));
});

test("coordinated crates resolve to exactly one version each, at the configured version", () => {
  const versionsByName = new Map();
  for (const { name, version } of recomputedFingerprint.packages) {
    const versions = versionsByName.get(name) ?? new Set();
    versions.add(version);
    versionsByName.set(name, versions);
  }

  const expectations = [
    [compilerStackConfig.rolldownCrate, compilerStackConfig.currentRolldownVersion],
    ["oxc_resolver", compilerStackConfig.currentResolverVersion],
    ...compilerStackConfig.oxcCrates.map((crate) => [crate, compilerStackConfig.currentOxcVersion]),
  ];

  for (const [name, expected] of expectations) {
    const versions = versionsByName.get(name);
    // Not every direct OXC crate is reachable from rolldown; assert only that
    // the reachable ones resolve uniquely to the coordinated version.
    if (!versions) {
      continue;
    }
    assert.deepEqual(
      [...versions],
      [expected],
      `${name} must resolve to exactly ${expected}, got ${[...versions].join(", ")}`,
    );
  }

  // The anchors of each line must be present at all.
  assert.ok(versionsByName.has(compilerStackConfig.rolldownCrate));
  assert.ok(versionsByName.has("oxc_parser"));
  assert.ok(versionsByName.has("oxc_resolver"));
});
