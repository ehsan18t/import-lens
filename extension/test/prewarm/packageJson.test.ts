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

test("prewarmPackageJsonDocuments sends file package.json documents under their analysis root", async () => {
  const sent: string[] = [];
  const packageJsonPath = path.join("workspace", "package.json");
  const secondFolderPackageJsonPath = path.join("other", "packages", "app", "package.json");
  const analysisRoots = new Map([
    [packageJsonPath, "workspace"],
    [secondFolderPackageJsonPath, "other"],
  ]);

  const count = await prewarmPackageJsonDocuments(
    [
      { uri: { scheme: "file", fsPath: packageJsonPath } },
      { uri: { scheme: "untitled", fsPath: path.join("workspace", "package.json") } },
      { uri: { scheme: "file", fsPath: path.join("workspace", "package-lock.json") } },
      { uri: { scheme: "file", fsPath: secondFolderPackageJsonPath } },
    ],
    {
      prewarmPackageJson: (packageJsonPath, activeDocumentPath, workspaceRoot) => {
        sent.push(`${packageJsonPath}:${activeDocumentPath}@${workspaceRoot}`);
      },
    },
    async (document) => analysisRoots.get(document.uri.fsPath) ?? "unexpected",
  );

  assert.equal(count, 2);
  assert.deepEqual(sent, [
    `${packageJsonPath}:${packageJsonPath}@workspace`,
    `${secondFolderPackageJsonPath}:${secondFolderPackageJsonPath}@other`,
  ]);
});
