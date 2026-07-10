import assert from "node:assert/strict";
import test from "node:test";
import { createStagedManifest, stagedPackageLayout } from "../package-vsix-manifest.mjs";

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

const { icon: _icon, ...manifestWithoutIcon } = manifest;

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

test("stagedPackageLayout stages only the daemon directory for the requested target", () => {
  const { copies } = stagedPackageLayout({ manifest, target: "win32-arm64" });
  const daemonCopies = copies.filter(({ source }) => source.startsWith("dist/bin"));

  assert.deepEqual(daemonCopies, [
    { source: "dist/bin/win32-arm64", destination: "dist/bin/win32-arm64" },
  ]);
});

test("stagedPackageLayout copies every non-generated entry in the manifest allowlist", () => {
  // package.json is written by createStagedManifest, never copied. Everything
  // else the manifest promises to ship must actually be staged.
  const { manifestFiles, copies } = stagedPackageLayout({ manifest, target: "win32-x64" });
  const copied = new Set(copies.map(({ destination }) => destination));

  for (const entry of manifestFiles) {
    if (entry === "package.json") {
      continue;
    }

    const normalized = entry.replace(/\/$/u, "");
    const staged = [...copied].some(
      (destination) => destination === normalized || destination.startsWith(`${normalized}/`),
    );

    assert.ok(staged, `manifest promises ${entry} but nothing copies it`);
  }
});

test("stagedPackageLayout includes the icon only when the manifest declares one", () => {
  const withIcon = stagedPackageLayout({ manifest, target: "linux-x64" });
  const withoutIcon = stagedPackageLayout({ manifest: manifestWithoutIcon, target: "linux-x64" });

  assert.ok(withIcon.copies.some(({ source }) => source === "media/icon.png"));
  assert.ok(withIcon.manifestFiles.includes("media/icon.png"));

  assert.ok(!withoutIcon.copies.some(({ source }) => source === "media/icon.png"));
  assert.ok(!withoutIcon.manifestFiles.includes("media/icon.png"));
});
