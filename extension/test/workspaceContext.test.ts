import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { analysisRootForFile } from "../src/workspaceContext.js";

test("analysisRootForFile prefers the VS Code workspace folder when available", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-context-"));

  try {
    const filePath = path.join(root, "packages", "app", "src", "index.ts");

    assert.equal(await analysisRootForFile(filePath, root), root);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("analysisRootForFile uses nearest package ancestor for loose files", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-context-"));

  try {
    const appRoot = path.join(root, "packages", "app");
    const filePath = path.join(appRoot, "src", "index.ts");
    await mkdir(path.join(appRoot, "src"), { recursive: true });
    await writeFile(path.join(appRoot, "package.json"), JSON.stringify({ type: "module" }), "utf8");

    assert.equal(await analysisRootForFile(filePath), appRoot);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("analysisRootForFile uses nearest node_modules ancestor for loose files", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-context-"));

  try {
    const appRoot = path.join(root, "standalone-app");
    const filePath = path.join(appRoot, "src", "index.ts");
    await mkdir(path.join(appRoot, "src"), { recursive: true });
    await mkdir(path.join(appRoot, "node_modules"), { recursive: true });

    assert.equal(await analysisRootForFile(filePath), appRoot);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("analysisRootForFile falls back to the file directory", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-context-"));

  try {
    const sourceDir = path.join(root, "src");
    const filePath = path.join(sourceDir, "index.ts");
    await mkdir(sourceDir, { recursive: true });

    assert.equal(await analysisRootForFile(filePath), sourceDir);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
