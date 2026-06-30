import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { oxcStackConfig } from "../oxc-stack.config.mjs";

const repoFile = (relativePath) => readFileSync(new URL(`../../${relativePath}`, import.meta.url), "utf8");

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

const escapedVersion = (version) => version.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
