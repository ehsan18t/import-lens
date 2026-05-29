import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import { isPackageJsonPath, packageJsonPrewarmPayload } from "../../src/prewarm/packageJsonHelpers.js";

test("isPackageJsonPath matches package.json exactly", () => {
  assert.equal(isPackageJsonPath(path.join("workspace", "package.json")), true);
  assert.equal(isPackageJsonPath(path.join("workspace", "package-lock.json")), false);
  assert.equal(isPackageJsonPath(path.join("workspace", "packages", "app", "package.json")), true);
});

test("packageJsonPrewarmPayload uses the package file as active document path", () => {
  const packageJsonPath = path.join("workspace", "packages", "app", "package.json");

  assert.deepEqual(packageJsonPrewarmPayload(packageJsonPath), {
    packageJsonPath,
    activeDocumentPath: packageJsonPath,
  });
});

test("packageJsonPrewarmPayload returns null for non-package files", () => {
  assert.equal(packageJsonPrewarmPayload(path.join("workspace", "src", "index.ts")), null);
});
