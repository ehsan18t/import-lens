import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { oxcStackConfig } from "../oxc-stack.config.mjs";

const repoFile = (relativePath) =>
  readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");
const REMOVED_EXTENSION_DEPS = ["p-" + "queue", "p-" + "timeout", "eventemitter" + "3"];

test("dependency policy pins the oxc analysis stack as one coordinated version", () => {
  const workspaceCargoToml = repoFile("Cargo.toml");
  const cargoToml = repoFile("daemon/Cargo.toml");
  const dockerfile = repoFile("Dockerfile.build");
  const rustToolchain = repoFile("rust-toolchain.toml");
  for (const crate of oxcStackConfig.oxcCrates) {
    assert.match(
      cargoToml,
      new RegExp(`^${crate} = "~${escapedVersion(oxcStackConfig.currentOxcVersion)}"$`, "mu"),
    );
  }

  assert.doesNotMatch(cargoToml, /^oxc_mangler = /mu);
  assert.match(
    cargoToml,
    new RegExp(
      `^oxc_resolver = "~${escapedVersion(oxcStackConfig.currentResolverVersion)}"$`,
      "mu",
    ),
  );
  assert.doesNotMatch(workspaceCargoToml, /^rust-version = /mu);
  assert.doesNotMatch(cargoToml, /^rust-version\.workspace = /mu);
  assert.match(dockerfile, /^ARG RUST_VERSION=stable$/mu);
  assert.match(dockerfile, /^ARG ZIG_VERSION=latest$/mu);
  assert.match(dockerfile, /^ARG CARGO_ZIGBUILD_VERSION=latest$/mu);
  assert.match(dockerfile, /https:\/\/ziglang\.org\/download\/index\.json/);
  assert.match(dockerfile, /ln -sf \/opt\/zig\/zig \/usr\/local\/bin\/zig/);
  assert.match(dockerfile, /cargo install cargo-zigbuild --locked/);
  assert.match(
    dockerfile,
    /cargo install cargo-zigbuild --version "\$\{CARGO_ZIGBUILD_VERSION\}" --locked/,
  );
  assert.doesNotMatch(dockerfile, /ZIG_VERSION=0\./);
  assert.doesNotMatch(dockerfile, /CARGO_ZIGBUILD_VERSION=0\./);
  assert.match(rustToolchain, /^channel = "stable"$/mu);
  const manifest = JSON.parse(repoFile("package.json"));
  assert.equal(manifest.dependencies["oxc-parser"], undefined);
  assert.equal(manifest.scripts["deps:update:oxc"], "node scripts/update-oxc-stack.mjs");
  // Range-respecting refresh, not `--latest` (which would ignore the ranges).
  assert.equal(manifest.scripts["deps:update:safe"], "pnpm update && cargo update");
  // The redundant `deps:update` alias was removed; the oxc updater must not re-add it.
  assert.equal(manifest.scripts["deps:update"], undefined);
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

  // Version constraints follow the blast-radius policy.
  // Exact pin: @types/vscode tracks the engines.vscode floor (min supported API),
  // never the latest — floating it up would let us call APIs absent in old VS Code.
  assert.equal(manifest.devDependencies["@types/vscode"], "1.90.0");
  // Tilde (patch-only): a typescript minor can add stricter checks that break tsc.
  assert.match(manifest.devDependencies.typescript, /^~6[.]/u);
  // Caret: dev tooling and well-behaved libs stay current; a break is caught in
  // CI, never shipped, and the lockfile holds the build steady between deliberate
  // updates. For the 0.x tools (esbuild, tsdown) caret is effectively patch-only.
  // tsdown bundles the extension, vsce packages the VSIX, esbuild backs the
  // accuracy comparator — all left to float rather than frozen exact.
  assert.match(manifest.devDependencies.esbuild, /^\^/u);
  assert.match(manifest.devDependencies.tsdown, /^\^/u);
  assert.match(manifest.devDependencies["@vscode/vsce"], /^\^/u);
  assert.match(manifest.devDependencies["@biomejs/biome"], /^\^/u);
  assert.match(manifest.devDependencies.lefthook, /^\^/u);
  assert.match(manifest.devDependencies.ovsx, /^\^/u);
  assert.match(manifest.dependencies["@msgpack/msgpack"], /^\^/u);
  // @types/node tracks the Node 24 toolchain, minor+patch floating.
  assert.match(manifest.devDependencies["@types/node"], /^\^24[.]/u);

  // PNPM_VERSION lives in every workflow that installs pnpm; keep them in lockstep.
  assert.match(validateWorkflow, /^ {2}PNPM_VERSION: 11[.]9[.]0$/mu);
  assert.match(buildWorkflow, /^ {2}PNPM_VERSION: 11[.]9[.]0$/mu);
  assert.match(releaseWorkflow, /^ {2}PNPM_VERSION: 11[.]9[.]0$/mu);
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
