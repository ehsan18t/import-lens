import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = () =>
  readFileSync(new URL("../../.github/workflows/release.yml", import.meta.url), "utf8");

const actionUses = (source) => source.match(/uses:\s+[\w-]+\/[\w-]+(?:\/[\w-]+)?@v[^\s]+/gu) ?? [];

test("release workflow pins current action versions exactly", () => {
  const source = workflow();

  assert.match(source, /actions\/checkout@v7\.0\.0/u);
  assert.match(source, /pnpm\/action-setup@v6\.0\.9/u);
  assert.match(source, /actions\/setup-node@v6\.4\.0/u);
  assert.match(source, /actions\/download-artifact@v8\.0\.1/u);
  assert.match(source, /taiki-e\/install-action@v2\.82\.7/u);

  for (const uses of actionUses(source)) {
    assert.match(uses, /@v\d+\.\d+\.\d+$/u);
  }
});

test("release workflow drafts GitHub releases and conditionally publishes to both stores", () => {
  const source = workflow();

  assert.match(source, /gh release create/u);
  assert.match(source, /--draft/u);
  assert.match(source, /--notes-file notes\.md/u);

  assert.match(source, /VSCE_PAT/u);
  assert.match(source, /publish-vsix\.mjs vsce dist\/vsix\/\*\.vsix/u);
  assert.match(source, /OVSX_PAT/u);
  assert.match(source, /publish-vsix\.mjs ovsx dist\/vsix\/\*\.vsix/u);

  // Publishing is gated on explicit per-run selection, not just secret presence.
  assert.match(source, /inputs\.publish_vscode/u);
  assert.match(source, /inputs\.publish_openvsx/u);
  assert.match(source, /inputs\.dry_run/u);
});

test("release workflow is safe to re-run after a partially published release", () => {
  const source = workflow();

  // A tag that already has a release is refreshed, not recreated. Re-creating
  // it fails with "release already exists" and strands the retry.
  assert.match(source, /gh release view "\$RELEASE_TAG"/u);
  assert.match(source, /gh release edit "\$RELEASE_TAG"/u);
  assert.match(source, /gh release upload "\$RELEASE_TAG" dist\/vsix\/\*\.vsix --clobber/u);

  // Store publishing goes through the script that passes --skip-duplicate and
  // retries, never a bare loop that aborts on the first failing target.
  assert.doesNotMatch(source, /pnpm exec (vsce|ovsx) publish/u);
  assert.doesNotMatch(source, /for file in dist\/vsix/u);

  // The PATs reach the CLIs through the environment, never through argv.
  assert.doesNotMatch(source, /--pat/u);
});

test("release workflow fails fast in preflight on missing store secrets or no destination", () => {
  const source = workflow();

  assert.match(source, /No destination selected/u);
  assert.match(source, /VSCE_PAT secret is not configured/u);
  assert.match(source, /OVSX_PAT secret is not configured/u);
});

test("release workflow resolves an optional version and locates the build by artifact", () => {
  const source = workflow();

  assert.match(source, /required: false/u);
  assert.match(source, /resolve-version\.mjs/u);
  assert.match(source, /RELEASE_VERSION=\$version/u);

  // The build run is correlated via its version-stamped artifact, not run-name.
  assert.match(source, /actions\/artifacts\?name=/u);
  assert.doesNotMatch(source, /gh run list/u);

  // The old hard equality guard against package.json is gone.
  assert.doesNotMatch(source, /does not match release input/u);
});

test("release workflow generates the changelog with full history and no wasm targets", () => {
  const source = workflow();

  assert.match(source, /generate-changelog\.mjs/u);
  assert.match(source, /fetch-depth: 0/u);
  assert.doesNotMatch(source, /wasm/iu);
});
