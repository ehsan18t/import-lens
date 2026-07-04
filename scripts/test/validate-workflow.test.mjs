import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = () => readFileSync(new URL("../../.github/workflows/validate.yml", import.meta.url), "utf8");

const actionUses = (source) =>
  source.match(/uses:\s+[\w-]+\/[\w-]+(?:\/[\w-]+)?@v[^\s]+/gu) ?? [];

test("validate is a reusable workflow with opt-in heavy gates", () => {
  const source = workflow();

  assert.match(source, /on:\s*\n\s*workflow_call:/u);
  assert.match(source, /run_accuracy:/u);
  assert.match(source, /run_performance:/u);
  assert.match(source, /run_coverage:/u);

  // Each heavy gate is guarded by its toggle so callers run only what they need.
  assert.match(source, /if: \$\{\{ inputs\.run_accuracy \}\}/u);
  assert.match(source, /if: \$\{\{ inputs\.run_performance \}\}/u);
  assert.match(source, /if: \$\{\{ inputs\.run_coverage \}\}/u);
});

test("validate resolves and exposes the version for callers", () => {
  const source = workflow();

  assert.match(source, /resolve-version\.mjs/u);
  assert.match(source, /version: \$\{\{ steps\.resolve\.outputs\.version \}\}/u);
  assert.match(source, /value: \$\{\{ jobs\.validate\.outputs\.version \}\}/u);
});

test("validate uses the latest stable Rust, not a fixed version", () => {
  const source = workflow();

  assert.match(source, /rustup toolchain install stable/u);
  assert.doesNotMatch(source, /RUST_VERSION/u);
  assert.doesNotMatch(source, /1\.89\.0/u);
});

test("validate caches Rust builds and installs cargo-llvm-cov from a prebuilt binary", () => {
  const source = workflow();

  assert.match(source, /Swatinem\/rust-cache@v2\.9\.1/u);
  assert.match(source, /taiki-e\/install-action@v[\d.]+\n\s+with:\n\s+tool: cargo-llvm-cov@0\.8\.7/u);

  // The from-source compile is gone.
  assert.doesNotMatch(source, /cargo install cargo-llvm-cov/u);
});

test("validate workflow pins current action versions exactly", () => {
  const source = workflow();

  assert.match(source, /actions\/checkout@v7\.0\.0/u);
  assert.match(source, /pnpm\/action-setup@v6\.0\.9/u);
  assert.match(source, /actions\/setup-node@v6\.4\.0/u);

  for (const uses of actionUses(source)) {
    assert.match(uses, /@v\d+\.\d+\.\d+$/u);
  }
});
