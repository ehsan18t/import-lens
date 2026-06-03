import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { resolveInstalledPackage } from "../../src/imports/resolver.js";

test("resolveInstalledPackage reads the nearest package version from node_modules", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-resolver-"));

  try {
    const appDir = path.join(root, "packages", "app");
    const packageDir = path.join(appDir, "node_modules", "lodash-es");
    await mkdir(packageDir, { recursive: true });
    await writeFile(path.join(packageDir, "package.json"), JSON.stringify({ version: "4.17.21" }), "utf8");

    const result = await resolveInstalledPackage("lodash-es/debounce", path.join(appDir, "src", "index.ts"));

    assert.deepEqual(result, {
      ok: true,
      packageName: "lodash-es",
      packageJsonPath: path.join(packageDir, "package.json"),
      packageRoot: packageDir,
      version: "4.17.21",
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("resolveInstalledPackage handles scoped packages", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-resolver-"));

  try {
    const packageDir = path.join(root, "node_modules", "@tanstack", "react-query");
    await mkdir(packageDir, { recursive: true });
    await writeFile(path.join(packageDir, "package.json"), JSON.stringify({ version: "5.28.0" }), "utf8");

    const result = await resolveInstalledPackage("@tanstack/react-query/build", path.join(root, "src", "index.ts"));

    assert.equal(result.ok, true);
    assert.equal(result.ok ? result.packageName : "", "@tanstack/react-query");
    assert.equal(result.ok ? result.version : "", "5.28.0");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("resolveInstalledPackage reports package_not_found without throwing", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-resolver-"));

  try {
    const result = await resolveInstalledPackage("missing-pkg", path.join(root, "src", "index.ts"));

    assert.deepEqual(result, {
      ok: false,
      packageName: "missing-pkg",
      reason: "package_not_found",
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("resolveInstalledPackage keeps malformed package manifest requestable for daemon fallback", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-resolver-"));

  try {
    const packageDir = path.join(root, "node_modules", "broken-json");
    await mkdir(packageDir, { recursive: true });
    await writeFile(path.join(packageDir, "package.json"), "{ invalid json", "utf8");

    const result = await resolveInstalledPackage("broken-json", path.join(root, "src", "index.ts"));

    assert.deepEqual(result, {
      ok: true,
      packageName: "broken-json",
      packageJsonPath: path.join(packageDir, "package.json"),
      packageRoot: packageDir,
      version: "unknown",
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("resolveInstalledPackage keeps versionless package manifest requestable for daemon fallback", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-resolver-"));

  try {
    const packageDir = path.join(root, "node_modules", "versionless");
    await mkdir(packageDir, { recursive: true });
    await writeFile(path.join(packageDir, "package.json"), JSON.stringify({ module: "index.js" }), "utf8");

    const result = await resolveInstalledPackage("versionless", path.join(root, "src", "index.ts"));

    assert.deepEqual(result, {
      ok: true,
      packageName: "versionless",
      packageJsonPath: path.join(packageDir, "package.json"),
      packageRoot: packageDir,
      version: "unknown",
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
