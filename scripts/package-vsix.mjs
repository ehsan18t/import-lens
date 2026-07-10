#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createStagedManifest, stagedPackageLayout } from "./package-vsix-manifest.mjs";
import { stagingDir, targetInfo, vsixNameForTarget } from "./targets.mjs";

const require = createRequire(import.meta.url);

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const target = process.argv[2];

const fail = (message) => {
  console.error(message);
  process.exit(1);
};

const assertInsideRepo = (targetPath) => {
  const relativePath = path.relative(repoRoot, targetPath);

  if (relativePath.startsWith("..") || path.isAbsolute(relativePath)) {
    fail(`Refusing to write outside repository: ${targetPath}`);
  }
};

const copyPath = (sourcePath, destinationPath) => {
  assertInsideRepo(destinationPath);
  mkdirSync(path.dirname(destinationPath), { recursive: true });
  cpSync(sourcePath, destinationPath, {
    dereference: true,
    force: true,
    recursive: true,
  });
};

const run = (command, args, cwd) => {
  const needsShell = process.platform === "win32" && command === "pnpm";
  const result = needsShell
    ? spawnSync(`${command} ${args.join(" ")}`, { cwd, shell: true, stdio: "inherit" })
    : spawnSync(command, args, { cwd, stdio: "inherit" });

  if (result.error) {
    fail(result.error.message);
  }

  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} failed with exit code ${result.status ?? "unknown"}`);
  }
};

if (!target) {
  fail("Usage: node scripts/package-vsix.mjs <target>");
}

targetInfo(target);

const manifestPath = path.join(repoRoot, "package.json");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const stagingRoot = path.join(repoRoot, stagingDir, target);
const outputPath = path.join(repoRoot, vsixNameForTarget(manifest, target));

// Resolve vsce binary *before* the staging pnpm install, which may strip devDependencies.
const vsceBinary = require.resolve("@vscode/vsce/vsce");

assertInsideRepo(stagingRoot);
assertInsideRepo(outputPath);

rmSync(stagingRoot, { force: true, recursive: true });
rmSync(outputPath, { force: true });
mkdirSync(stagingRoot, { recursive: true });
mkdirSync(path.dirname(outputPath), { recursive: true });

writeFileSync(
  path.join(stagingRoot, "package.json"),
  `${JSON.stringify(createStagedManifest({ manifest }), null, 2)}\n`,
);

console.log("Installing production dependencies inside staging directory...");
run(
  "pnpm",
  ["install", "--prod", "--force", "--no-lockfile", "--node-linker=hoisted", "--ignore-workspace"],
  stagingRoot,
);

for (const { source, destination } of stagedPackageLayout({ manifest, target }).copies) {
  const sourcePath = path.join(repoRoot, source);

  if (!existsSync(sourcePath)) {
    fail(`Cannot stage ${source} into the VSIX: the path does not exist.`);
  }

  copyPath(sourcePath, path.join(stagingRoot, destination));
}

const result = spawnSync(
  process.execPath,
  [vsceBinary, "package", "--target", target, "--out", outputPath, "--allow-missing-repository"],
  {
    cwd: stagingRoot,
    stdio: "inherit",
  },
);

if (result.status !== 0) {
  fail(`vsce package failed with exit code ${result.status ?? "unknown"}`);
}

rmSync(stagingRoot, { force: true, recursive: true });
console.log(`Packaged ${outputPath}`);
