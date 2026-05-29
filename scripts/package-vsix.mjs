#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const target = process.argv[2];
const platformBindings = new Map([
  ["win32-x64", "@oxc-parser/binding-win32-x64-msvc"],
  ["win32-arm64", "@oxc-parser/binding-win32-arm64-msvc"],
  ["linux-x64", "@oxc-parser/binding-linux-x64-gnu"],
  ["linux-arm64", "@oxc-parser/binding-linux-arm64-gnu"],
  ["darwin-x64", "@oxc-parser/binding-darwin-x64"],
  ["darwin-arm64", "@oxc-parser/binding-darwin-arm64"],
]);

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

const packageRoot = (packageName, paths) =>
  path.dirname(require.resolve(`${packageName}/package.json`, { paths }));

const createStagedManifest = (manifest, bindingPackage) => ({
  ...manifest,
  dependencies: {
    [bindingPackage]: manifest.dependencies[bindingPackage],
    "oxc-parser": manifest.dependencies["oxc-parser"],
  },
  devDependencies: undefined,
  files: [
    "extension/dist/extension.cjs",
    "bin/",
    "node_modules/oxc-parser/",
    `node_modules/@oxc-parser/${path.basename(bindingPackage)}/`,
    "node_modules/@oxc-project/types/",
    "README.md",
    "package.json",
  ],
  scripts: undefined,
});

if (!target) {
  fail("Usage: node scripts/package-vsix.mjs <target>");
}

const bindingPackage = platformBindings.get(target);

if (!bindingPackage) {
  fail(`Unsupported VSIX target: ${target}`);
}

const manifestPath = path.join(repoRoot, "package.json");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const stagingRoot = path.join(repoRoot, ".vsix-staging", target);
const outputPath = path.join(repoRoot, `${manifest.name}-${target}-${manifest.version}.vsix`);

assertInsideRepo(stagingRoot);
assertInsideRepo(outputPath);

rmSync(stagingRoot, { force: true, recursive: true });
rmSync(outputPath, { force: true });
mkdirSync(stagingRoot, { recursive: true });

writeFileSync(
  path.join(stagingRoot, "package.json"),
  `${JSON.stringify(createStagedManifest(manifest, bindingPackage), null, 2)}\n`,
);

console.log(`Downloading ${bindingPackage} inside staging directory...`);
const npmResult = spawnSync(
  "npm",
  ["install", "--no-save", `${bindingPackage}@${manifest.dependencies["oxc-parser"]}`],
  {
    cwd: stagingRoot,
    stdio: "inherit",
    shell: true,
  }
);

if (npmResult.status !== 0) {
  fail(`Failed to download ${bindingPackage}`);
}

copyPath(path.join(repoRoot, "README.md"), path.join(stagingRoot, "README.md"));
copyPath(
  path.join(repoRoot, "extension", "dist", "extension.cjs"),
  path.join(stagingRoot, "extension", "dist", "extension.cjs"),
);
copyPath(path.join(repoRoot, "bin", target), path.join(stagingRoot, "bin", target));
const oxcParserRoot = packageRoot("oxc-parser", [repoRoot]);

copyPath(oxcParserRoot, path.join(stagingRoot, "node_modules", "oxc-parser"));
// The bindingPackage was already downloaded into stagingRoot/node_modules by npm install!
copyPath(
  packageRoot("@oxc-project/types", [oxcParserRoot]),
  path.join(stagingRoot, "node_modules", "@oxc-project", "types"),
);

const vsceBinary = require.resolve("@vscode/vsce/vsce");
const result = spawnSync(
  process.execPath,
  [vsceBinary, "package", "--target", target, "--out", outputPath],
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
