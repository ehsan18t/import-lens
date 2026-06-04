#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createStagedManifest } from "./package-vsix-manifest.mjs";
import { targetInfo, vsixNameForTarget } from "./targets.mjs";

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

const bindingPackage = targetInfo(target).oxcParserBinding;

const manifestPath = path.join(repoRoot, "package.json");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const stagingRoot = path.join(repoRoot, ".vsix-staging", target);
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
  `${JSON.stringify(createStagedManifest({ manifest, bindingPackage }), null, 2)}\n`,
);

console.log(`Downloading ${bindingPackage} inside staging directory...`);
run("pnpm", ["install", "--prod", "--force", "--no-lockfile", "--node-linker=hoisted", "--ignore-workspace"], stagingRoot);

copyPath(path.join(repoRoot, "README.md"), path.join(stagingRoot, "README.md"));
copyPath(path.join(repoRoot, "LICENSE"), path.join(stagingRoot, "LICENSE"));
copyPath(
  path.join(repoRoot, "extension", "dist", "extension.cjs"),
  path.join(stagingRoot, "extension", "dist", "extension.cjs"),
);
copyPath(path.join(repoRoot, "bin", target), path.join(stagingRoot, "bin", target));

if (manifest.icon) {
  const iconPath = path.join(repoRoot, manifest.icon);

  if (!existsSync(iconPath)) {
    fail(`Extension icon is declared at ${manifest.icon}, but the file does not exist.`);
  }

  copyPath(iconPath, path.join(stagingRoot, manifest.icon));
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
