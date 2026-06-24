import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { createStagedManifest } from "./package-vsix-manifest.mjs";

const packageVsixScript = readFileSync(new URL("./package-vsix.mjs", import.meta.url), "utf8");

const manifest = {
  name: "import-lens",
  version: "0.1.0",
  icon: "media/icon.png",
  dependencies: {
    "@msgpack/msgpack": "3.1.3",
  },
  devDependencies: {
    typescript: "6.0.3",
  },
  scripts: {
    build: "tsdown",
  },
};

test("createStagedManifest includes the repository license in packaged files", () => {
  const staged = createStagedManifest({ manifest });

  assert.ok(staged.files.includes("LICENSE"));
  assert.ok(staged.files.includes("cli/"));
});

test("createStagedManifest keeps production dependencies and strips development-only fields", () => {
  const staged = createStagedManifest({ manifest });

  assert.deepEqual(staged.dependencies, { "@msgpack/msgpack": "3.1.3" });
  assert.equal(staged.devDependencies, undefined);
  assert.equal(staged.scripts, undefined);
});

test("package-vsix copies every non-generated manifest directory", () => {
  assert.match(packageVsixScript, /copyPath\(path\.join\(repoRoot, "cli"\), path\.join\(stagingRoot, "cli"\)\)/);
});
