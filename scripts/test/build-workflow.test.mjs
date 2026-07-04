import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = () => readFileSync(new URL("../../.github/workflows/build.yml", import.meta.url), "utf8");

const actionUses = (source) =>
  source.match(/uses:\s+[\w-]+\/[\w-]+(?:\/[\w-]+)?@v[^\s]+/gu) ?? [];

test("build workflow packages every native VSIX target", () => {
  const source = workflow();

  for (const target of [
    "win32-x64",
    "win32-arm64",
    "linux-x64",
    "linux-arm64",
    "darwin-x64",
    "darwin-arm64",
  ]) {
    assert.match(source, new RegExp(`target: ${target}\\b`, "u"));
  }

  // Packaging is invoked generically per matrix target.
  assert.match(source, /pnpm run package:\$\{\{ matrix\.target \}\}/u);

  assert.doesNotMatch(source, /wasm/iu);
  assert.doesNotMatch(source, /zigbuild/iu);
  assert.doesNotMatch(source, /docker/iu);
});

test("build workflow builds each target on its native OS", () => {
  const source = workflow();

  assert.match(source, /runner: windows-latest/u);
  assert.match(source, /runner: ubuntu-24\.04\b/u);
  assert.match(source, /runner: ubuntu-24\.04-arm/u);
  assert.match(source, /runner: macos-latest/u);
});

test("build workflow caches each VSIX per target and resolved version and supports force rebuild", () => {
  const source = workflow();

  assert.match(source, /key: vsix-\$\{\{ matrix\.target \}\}-\$\{\{ needs\.validate\.outputs\.version \}\}/u);
  assert.match(source, /inputs\.force/u);
  assert.match(source, /retention-days: 1/u);
  assert.match(source, /if-no-files-found: error/u);
});

test("build workflow validates via the reusable workflow and overrides package.json before packaging", () => {
  const source = workflow();

  // Validation (including resolving the version) is delegated to the reusable
  // workflow, which feeds its resolved version back into the matrix.
  assert.match(source, /uses: \.\/\.github\/workflows\/validate\.yml/u);
  assert.match(source, /requested_version: \$\{\{ inputs\.version \}\}/u);
  assert.match(source, /needs\.validate\.outputs\.version/u);

  // The optional input still overrides package.json at package time.
  assert.match(source, /required: false/u);
  assert.match(source, /set-version\.mjs/u);

  // The old hard equality guard against package.json is gone.
  assert.doesNotMatch(source, /does not match build input/u);
});

test("build workflow caches the Rust compile per target and tracks latest stable", () => {
  const source = workflow();

  assert.match(source, /Swatinem\/rust-cache@v2\.9\.1/u);
  assert.match(source, /rustup toolchain install stable/u);
  assert.doesNotMatch(source, /RUST_VERSION/u);
});

test("build workflow cancels an in-progress run when a new one starts", () => {
  const source = workflow();

  assert.match(source, /group: \$\{\{ github\.workflow \}\}/u);
  assert.match(source, /cancel-in-progress: true/u);
});

test("build workflow pins current action versions exactly", () => {
  const source = workflow();

  assert.match(source, /actions\/checkout@v7\.0\.0/u);
  assert.match(source, /pnpm\/action-setup@v6\.0\.9/u);
  assert.match(source, /actions\/setup-node@v6\.4\.0/u);
  assert.match(source, /actions\/cache\/(?:restore|save)@v6\.1\.0/u);
  assert.match(source, /actions\/upload-artifact@v7\.0\.1/u);

  for (const uses of actionUses(source)) {
    assert.match(uses, /@v\d+\.\d+\.\d+$/u);
  }
});
