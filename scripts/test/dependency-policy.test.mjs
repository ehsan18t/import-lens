import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { oxcStackConfig } from "../oxc-stack.config.mjs";

const repoFile = (relativePath) => readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");
const REMOVED_EXTENSION_DEPS = ["p-" + "queue", "p-" + "timeout", "eventemitter" + "3"];

test("dependency policy pins the oxc analysis stack as one coordinated version", () => {
  const workspaceCargoToml = repoFile("Cargo.toml");
  const cargoToml = repoFile("daemon/Cargo.toml");
  const dockerfile = repoFile("Dockerfile.build");
  const rustToolchain = repoFile("rust-toolchain.toml");
  for (const crate of oxcStackConfig.oxcCrates) {
    assert.match(cargoToml, new RegExp(`^${crate} = "=${escapedVersion(oxcStackConfig.currentOxcVersion)}"$`, "mu"));
  }

  assert.doesNotMatch(cargoToml, /^oxc_mangler = /mu);
  assert.match(cargoToml, new RegExp(`^oxc_resolver = "=${escapedVersion(oxcStackConfig.currentResolverVersion)}"$`, "mu"));
  assert.doesNotMatch(workspaceCargoToml, /^rust-version = /mu);
  assert.doesNotMatch(cargoToml, /^rust-version\.workspace = /mu);
  assert.match(dockerfile, /^ARG RUST_VERSION=stable$/mu);
  assert.match(dockerfile, /^ARG ZIG_VERSION=latest$/mu);
  assert.match(dockerfile, /^ARG CARGO_ZIGBUILD_VERSION=latest$/mu);
  assert.match(dockerfile, /https:\/\/ziglang\.org\/download\/index\.json/);
  assert.match(dockerfile, /ln -sf \/opt\/zig\/zig \/usr\/local\/bin\/zig/);
  assert.match(dockerfile, /cargo install cargo-zigbuild --locked/);
  assert.match(dockerfile, /cargo install cargo-zigbuild --version "\$\{CARGO_ZIGBUILD_VERSION\}" --locked/);
  assert.doesNotMatch(dockerfile, /ZIG_VERSION=0\./);
  assert.doesNotMatch(dockerfile, /CARGO_ZIGBUILD_VERSION=0\./);
  assert.match(rustToolchain, /^channel = "stable"$/mu);
  const manifest = JSON.parse(repoFile("package.json"));
  assert.equal(manifest.dependencies["oxc-parser"], undefined);
  assert.equal(manifest.scripts["deps:update"], "pnpm deps:update:oxc");
  assert.equal(manifest.scripts["deps:update:oxc"], "node scripts/update-oxc-stack.mjs");
  assert.equal(manifest.scripts["deps:update:all"], "pnpm update --latest && cargo update");
});

test("dependency policy pins build tooling and removes stale extension-host queue deps", () => {
  const manifest = JSON.parse(repoFile("package.json"));
  // CI delegates its toolchain pins to the reusable validate workflow.
  const validateWorkflow = repoFile(".github/workflows/validate.yml");
  const buildWorkflow = repoFile(".github/workflows/build.yml");
  const releaseWorkflow = repoFile(".github/workflows/release.yml");
  const dockerfile = repoFile("Dockerfile.build");
  const tsdownConfig = repoFile("tsdown.config.ts");

  assert.match(manifest.packageManager, /^pnpm@11[.]9[.]0[+]sha512[.]/u);
  assert.equal(manifest.devDependencies.esbuild, "0.28.1");
  assert.equal(manifest.devDependencies.tsdown, "0.22.3");
  assert.equal(manifest.devDependencies["@vscode/vsce"], "3.9.2");

  // PNPM_VERSION lives in every workflow that installs pnpm; keep them in lockstep.
  assert.match(validateWorkflow, /^  PNPM_VERSION: 11[.]9[.]0$/mu);
  assert.match(buildWorkflow, /^  PNPM_VERSION: 11[.]9[.]0$/mu);
  assert.match(releaseWorkflow, /^  PNPM_VERSION: 11[.]9[.]0$/mu);
  assert.match(validateWorkflow, /node-version: 24/u);
  assert.match(releaseWorkflow, /node-version: 24/u);
  assert.doesNotMatch(validateWorkflow, new RegExp(`node-version: ${22}`, "u"));
  assert.doesNotMatch(releaseWorkflow, new RegExp(`node-version: ${22}`, "u"));

  assert.match(dockerfile, /^FROM node:24-bookworm$/mu);
  assert.match(dockerfile, /^ARG PNPM_VERSION=11[.]9[.]0$/mu);
  assert.match(dockerfile, /Expected Node 24[.]11[+] build image/u);
  assert.match(tsdownConfig, /target: "node20"/u);
  assert.match(tsdownConfig, /platform: "node"/u);

  for (const dependency of REMOVED_EXTENSION_DEPS) {
    assert.equal(manifest.dependencies[dependency], undefined);
    assert.equal(manifest.devDependencies[dependency], undefined);
    assert.doesNotMatch(tsdownConfig, new RegExp(escapedVersion(dependency), "u"));
  }
});

const escapedVersion = (version) => version.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
