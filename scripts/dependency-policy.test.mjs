import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const repoFile = (relativePath) => readFileSync(new URL(`../${relativePath}`, import.meta.url), "utf8");

test("dependency policy keeps parser and build tooling upgrade-friendly", () => {
  const workspaceCargoToml = repoFile("Cargo.toml");
  const cargoToml = repoFile("daemon/Cargo.toml");
  const dockerfile = repoFile("Dockerfile.build");
  const manifest = JSON.parse(repoFile("package.json"));
  const rustToolchain = repoFile("rust-toolchain.toml");
  const oxcCrates = [
    "oxc_allocator",
    "oxc_ast",
    "oxc_codegen",
    "oxc_mangler",
    "oxc_minifier",
    "oxc_parser",
    "oxc_semantic",
    "oxc_span",
    "oxc_syntax",
    "oxc_transformer",
  ];

  for (const crate of oxcCrates) {
    assert.match(cargoToml, new RegExp(`^${crate} = "\\^0"$`, "mu"));
  }

  assert.match(cargoToml, /^oxc_resolver = "\^11"$/mu);
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
  assert.equal(manifest.dependencies["oxc-parser"], "^0");
  assert.equal(manifest.scripts["deps:update"], "pnpm update --latest && cargo update");
});
