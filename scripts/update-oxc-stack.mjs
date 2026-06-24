#!/usr/bin/env node

import { execFile as execFileCallback } from "node:child_process";
import { readFile as fsReadFile, writeFile as fsWriteFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { oxcStackConfig } from "./oxc-stack.config.mjs";
import {
  formatOxcUpdateResult,
  latestCrateVersion,
  replaceKnownVersions,
  updateCargoToml,
  updateConfig,
  updateManifest,
  validateAvailableVersions,
  validateCurrentStack,
  validateVersion,
} from "./oxc-stack-helpers.mjs";

const execFilePromise = promisify(execFileCallback);

const defaultPaths = {
  cargoToml: "daemon/Cargo.toml",
  manifest: "package.json",
  dependencyPolicyTest: "scripts/dependency-policy.test.mjs",
  packageVsixManifestTest: "scripts/package-vsix-manifest.test.mjs",
  srs: "docs/ImportLens-SRS.md",
  config: "scripts/oxc-stack.config.mjs",
};

export const parseUpdateArgs = (argv) => {
  const parsed = {
    dryRun: false,
    oxcVersion: undefined,
    resolverVersion: undefined,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--dry-run") {
      parsed.dryRun = true;
      continue;
    }
    if (arg === "--oxc") {
      parsed.oxcVersion = valueAfter(argv, index, arg);
      index += 1;
      continue;
    }
    if (arg === "--resolver") {
      parsed.resolverVersion = valueAfter(argv, index, arg);
      index += 1;
      continue;
    }
    if (arg === "--help" || arg === "-h") {
      parsed.help = true;
      continue;
    }
    throw new Error(`Unknown option: ${arg}`);
  }

  return parsed;
};

export const updateOxcStack = async ({
  rootDir = process.cwd(),
  paths = defaultPaths,
  dryRun = false,
  oxcVersion,
  resolverVersion,
  fetchJson = defaultFetchJson,
  readFile = fsReadFile,
  writeFile = fsWriteFile,
  execFile = execFilePromise,
  stdout = undefined,
} = {}) => {
  const absolutePaths = Object.fromEntries(
    Object.entries({ ...defaultPaths, ...paths }).map(([key, relativePath]) => [
      key,
      path.join(rootDir, relativePath),
    ]),
  );

  const files = await readFiles(absolutePaths, readFile);
  validateCurrentStack(files.cargoToml, files.manifestJson);

  const targetOxcVersion = oxcVersion ?? (await latestCrateVersion(fetchJson, "oxc_parser"));
  const targetResolverVersion = resolverVersion ?? (await latestCrateVersion(fetchJson, "oxc_resolver"));

  validateVersion("OXC", targetOxcVersion);
  validateVersion("oxc_resolver", targetResolverVersion);
  await validateAvailableVersions(fetchJson, targetOxcVersion, targetResolverVersion);

  const nextFiles = {
    cargoToml: updateCargoToml(files.cargoToml, targetOxcVersion, targetResolverVersion),
    manifest: updateManifest(files.manifestJson, targetOxcVersion),
    dependencyPolicyTest: replaceKnownVersions(files.dependencyPolicyTest, targetOxcVersion, targetResolverVersion),
    packageVsixManifestTest: replaceKnownVersions(files.packageVsixManifestTest, targetOxcVersion, targetResolverVersion),
    srs: replaceKnownVersions(files.srs, targetOxcVersion, targetResolverVersion),
    config: updateConfig(files.config, targetOxcVersion, targetResolverVersion),
  };

  const changedFiles = Object.entries(nextFiles)
    .filter(([key, next]) => next !== files[key])
    .map(([key]) => paths[key] ?? defaultPaths[key]);

  if (!dryRun) {
    for (const [key, next] of Object.entries(nextFiles)) {
      if (next !== files[key]) {
        await writeFile(absolutePaths[key], next, "utf8");
      }
    }
    await updateLockfiles(execFile, targetOxcVersion, targetResolverVersion);
  }

  const result = {
    oxcVersion: targetOxcVersion,
    resolverVersion: targetResolverVersion,
    changedFiles,
    dryRun,
  };

  stdout?.write(formatOxcUpdateResult(result));
  return result;
};

const readFiles = async (absolutePaths, readFile) => {
  const manifest = await readText(readFile, absolutePaths.manifest);
  return {
    cargoToml: await readText(readFile, absolutePaths.cargoToml),
    manifest,
    manifestJson: JSON.parse(manifest),
    dependencyPolicyTest: await readText(readFile, absolutePaths.dependencyPolicyTest),
    packageVsixManifestTest: await readText(readFile, absolutePaths.packageVsixManifestTest),
    srs: await readText(readFile, absolutePaths.srs),
    config: await readText(readFile, absolutePaths.config),
  };
};

const readText = async (readFile, filePath) => {
  const content = await readFile(filePath, "utf8");
  return typeof content === "string" ? content : content.toString("utf8");
};

const valueAfter = (argv, index, option) => {
  const value = argv[index + 1];
  if (!value || value.startsWith("--")) {
    throw new Error(`${option} requires a version value`);
  }
  return value;
};

const updateLockfiles = async (execFile, oxcVersion, resolverVersion) => {
  await execFile("pnpm", ["install", "--lockfile-only"]);
  await execFile("cargo", ["update", "-p", "oxc_resolver", "--precise", resolverVersion]);
  for (const crate of oxcStackConfig.oxcCrates) {
    await execFile("cargo", ["update", "-p", crate, "--precise", oxcVersion]);
  }
};

const defaultFetchJson = async (url) => {
  const response = await fetch(url, {
    headers: {
      "User-Agent": "import-lens-oxc-updater",
      Accept: "application/json",
    },
  });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
};

const printHelp = () => {
  process.stdout.write(`Usage: pnpm deps:update:oxc -- [options]

Options:
  --oxc <version>       Target OXC monorepo version. Defaults to latest oxc_parser crate.
  --resolver <version>  Target oxc_resolver version. Defaults to latest stable crate.
  --dry-run            Validate and print planned file edits without writing.
  -h, --help           Show this help.
`);
};

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const args = parseUpdateArgs(process.argv.slice(2));
    if (args.help) {
      printHelp();
    } else {
      await updateOxcStack({ ...args, stdout: process.stdout });
    }
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
