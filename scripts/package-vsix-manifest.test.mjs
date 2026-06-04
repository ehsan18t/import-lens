import assert from "node:assert/strict";
import test from "node:test";
import { createStagedManifest } from "./package-vsix-manifest.mjs";

const manifest = {
  name: "import-lens",
  version: "0.1.0",
  icon: "media/icon.png",
  dependencies: {
    "oxc-parser": "^0",
  },
  devDependencies: {
    typescript: "6.0.3",
  },
  scripts: {
    build: "tsdown",
  },
};

test("createStagedManifest includes the repository license in packaged files", () => {
  const staged = createStagedManifest({
    manifest,
    bindingPackage: "@oxc-parser/binding-win32-x64-msvc",
  });

  assert.ok(staged.files.includes("LICENSE"));
});

test("createStagedManifest keeps target parser binding and strips development-only fields", () => {
  const staged = createStagedManifest({
    manifest,
    bindingPackage: "@oxc-parser/binding-win32-x64-msvc",
  });

  assert.equal(staged.dependencies["oxc-parser"], "^0");
  assert.equal(staged.dependencies["@oxc-parser/binding-win32-x64-msvc"], "^0");
  assert.equal(staged.devDependencies, undefined);
  assert.equal(staged.scripts, undefined);
});
