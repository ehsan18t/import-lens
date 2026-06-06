import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = () => readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8");

const actionUses = (source) =>
  source.match(/uses:\s+[\w-]+\/[\w-]+@v[^\s]+/gu) ?? [];

test("release workflow pins current action versions exactly", () => {
  const source = workflow();

  assert.match(source, /actions\/checkout@v6\.0\.3/u);
  assert.match(source, /pnpm\/action-setup@v6\.0\.8/u);
  assert.match(source, /actions\/setup-node@v6\.4\.0/u);
  assert.match(source, /actions\/upload-artifact@v7\.0\.1/u);
  assert.match(source, /actions\/download-artifact@v8\.0\.1/u);

  for (const uses of actionUses(source)) {
    assert.match(uses, /@v\d+\.\d+\.\d+$/u);
  }
});

test("release workflow drafts GitHub releases and conditionally publishes VSIXs", () => {
  const source = workflow();

  assert.match(source, /gh release create/u);
  assert.match(source, /--draft/u);
  assert.match(source, /VSCE_PAT/u);
  assert.match(source, /pnpm exec vsce publish --packagePath/u);
});

test("release workflow packages every native VSIX target", () => {
  const source = workflow();

  for (const target of [
    "win32-x64",
    "win32-arm64",
    "linux-x64",
    "linux-arm64",
    "darwin-x64",
    "darwin-arm64",
  ]) {
    assert.match(source, new RegExp(`package:${target}`, "u"));
  }

  assert.doesNotMatch(source, /wasm/iu);
});
