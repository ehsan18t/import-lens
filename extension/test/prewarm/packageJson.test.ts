import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import {
  isPackageJsonPath,
  packageJsonPrewarmPayload,
  prewarmPackageJsonDocuments,
} from "../../src/prewarm/packageJsonHelpers.js";

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

test("prewarmPackageJsonDocuments sends only file package.json documents", () => {
  const sent: string[] = [];
  const packageJsonPath = path.join("workspace", "package.json");
  const nestedPackageJsonPath = path.join("workspace", "packages", "app", "package.json");

  const count = prewarmPackageJsonDocuments(
    [
      { uri: { scheme: "file", fsPath: packageJsonPath } },
      { uri: { scheme: "untitled", fsPath: path.join("workspace", "package.json") } },
      { uri: { scheme: "file", fsPath: path.join("workspace", "package-lock.json") } },
      { uri: { scheme: "file", fsPath: nestedPackageJsonPath } },
    ],
    {
      prewarmPackageJson: (packageJsonPath, activeDocumentPath) => {
        sent.push(`${packageJsonPath}:${activeDocumentPath}`);
      },
    },
  );

  assert.equal(count, 2);
  assert.deepEqual(sent, [
    `${packageJsonPath}:${packageJsonPath}`,
    `${nestedPackageJsonPath}:${nestedPackageJsonPath}`,
  ]);
});
