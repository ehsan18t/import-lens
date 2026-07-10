import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { oxcStackConfig } from "../oxc-stack.config.mjs";

// Drift check. daemon/Cargo.toml and oxc-stack.config.mjs are two separately
// maintained records of the same version; `pnpm deps:update:oxc` writes both.
// These fail when only one of them moved -- a real bug, caught before it ships.
//
// oxc is the ONLY dependency whose version any test may assert. It is the only
// one where a bump can silently change analysis output. See the Testing Policy
// in CLAUDE.md before adding anything here.

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

const escapeVersion = (version) => version.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");

test("every oxc monorepo crate is pinned patch-only at the configured version", () => {
  const cargoToml = repoFile("daemon/Cargo.toml");

  for (const crate of oxcStackConfig.oxcCrates) {
    assert.match(
      cargoToml,
      new RegExp(`^${crate} = "~${escapeVersion(oxcStackConfig.currentOxcVersion)}"$`, "mu"),
      `${crate} must be pinned to ~${oxcStackConfig.currentOxcVersion}`,
    );
  }
});

test("oxc_resolver is pinned patch-only at its own configured version", () => {
  // Versioned independently of the monorepo crates, in a separate repository.
  assert.match(
    repoFile("daemon/Cargo.toml"),
    new RegExp(`^oxc_resolver = "~${escapeVersion(oxcStackConfig.currentResolverVersion)}"$`, "mu"),
  );
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
