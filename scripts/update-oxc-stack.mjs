#!/usr/bin/env node

import { execFile as execFileCallback } from "node:child_process";
import { readFile as fsReadFile, writeFile as fsWriteFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { oxcStackConfig } from "./oxc-stack.config.mjs";

const execFilePromise = promisify(execFileCallback);

const defaultPaths = {
  cargoToml: "daemon/Cargo.toml",
  manifest: "package.json",
  dependencyPolicyTest: "scripts/dependency-policy.test.mjs",
  packageVsixManifestTest: "scripts/package-vsix-manifest.test.mjs",
  srs: "docs/ImportLens-SRS.md",
  config: "scripts/oxc-stack.config.mjs",
};

const semverPattern = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u;

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

  const targetOxcVersion = oxcVersion ?? (await latestNpmVersion(fetchJson, "oxc-parser"));
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

  stdout?.write(formatResult(result));
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

const validateCurrentStack = (cargoToml, manifest) => {
  if (/^oxc_mangler\s*=/mu.test(cargoToml)) {
    throw new Error("oxc_mangler must not be present in daemon/Cargo.toml");
  }

  const crateVersions = oxcStackConfig.oxcCrates.map((crate) => {
    const match = cargoToml.match(new RegExp(`^${crate}\\s*=\\s*"([^"]+)"$`, "mu"));
    if (!match) {
      throw new Error(`Missing OXC crate pin: ${crate}`);
    }
    return match[1];
  });
  const uniqueCrateVersions = new Set(crateVersions);
  if (uniqueCrateVersions.size !== 1) {
    throw new Error(`Current OXC crate versions are not coordinated: ${[...uniqueCrateVersions].join(", ")}`);
  }

  const npmVersion = manifest.dependencies?.["oxc-parser"];
  if (npmVersion && npmVersion !== crateVersions[0]) {
    throw new Error(`Current npm oxc-parser version ${npmVersion} does not match OXC crates ${crateVersions[0]}`);
  }
};

const validateVersion = (label, version) => {
  if (!semverPattern.test(version)) {
    throw new Error(`Invalid ${label} version: ${version}`);
  }
};

const validateAvailableVersions = async (fetchJson, oxcVersion, resolverVersion) => {
  await npmPackageVersion(fetchJson, "oxc-parser", oxcVersion).catch((error) => {
    throw new Error(`Unavailable OXC version ${oxcVersion}: ${error.message}`);
  });

  await Promise.all(
    oxcStackConfig.oxcCrates.map((crate) =>
      crateVersion(fetchJson, crate, oxcVersion).catch((error) => {
        throw new Error(`Unavailable OXC crate ${crate}@${oxcVersion}: ${error.message}`);
      }),
    ),
  );

  await crateVersion(fetchJson, "oxc_resolver", resolverVersion).catch((error) => {
    throw new Error(`Unavailable oxc_resolver version ${resolverVersion}: ${error.message}`);
  });
};

const latestNpmVersion = async (fetchJson, packageName) => {
  const payload = await fetchJson(`https://registry.npmjs.org/${packageName}/latest`);
  if (!payload?.version) {
    throw new Error(`Could not resolve latest npm version for ${packageName}`);
  }
  return payload.version;
};

const latestCrateVersion = async (fetchJson, crate) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}`);
  const version = payload?.crate?.max_stable_version ?? payload?.crate?.newest_version;
  if (!version) {
    throw new Error(`Could not resolve latest crate version for ${crate}`);
  }
  return version;
};

const npmPackageVersion = async (fetchJson, packageName, version) => {
  const payload = await fetchJson(`https://registry.npmjs.org/${packageName}/${version}`);
  if (payload?.version !== version) {
    throw new Error(`registry returned ${payload?.version ?? "no version"}`);
  }
};

const crateVersion = async (fetchJson, crate, version) => {
  const payload = await fetchJson(`https://crates.io/api/v1/crates/${crate}/${version}`);
  const returnedVersion = payload?.version?.num;
  if (returnedVersion !== version) {
    throw new Error(`crates.io returned ${returnedVersion ?? "no version"}`);
  }
};

const updateCargoToml = (cargoToml, oxcVersion, resolverVersion) => {
  let next = cargoToml;
  for (const crate of oxcStackConfig.oxcCrates) {
    next = next.replace(new RegExp(`^${crate}\\s*=\\s*"[^"]+"$`, "gmu"), `${crate} = "${oxcVersion}"`);
  }
  return next.replace(/^oxc_resolver\s*=\s*"[^"]+"$/gmu, `oxc_resolver = "${resolverVersion}"`);
};

const updateManifest = (manifest, oxcVersion) => {
  const next = structuredClone(manifest);
  next.dependencies = next.dependencies ?? {};
  next.dependencies["oxc-parser"] = oxcVersion;

  for (const section of ["dependencies", "devDependencies", "optionalDependencies"]) {
    for (const name of Object.keys(next[section] ?? {})) {
      if (name.startsWith("@oxc-parser/binding-")) {
        next[section][name] = oxcVersion;
      }
    }
  }

  next.scripts = {
    ...(next.scripts ?? {}),
    "deps:update": "pnpm deps:update:oxc",
    "deps:update:oxc": "node scripts/update-oxc-stack.mjs",
    "deps:update:all": "pnpm update --latest && cargo update",
  };

  return `${JSON.stringify(next, null, 2)}\n`;
};

const replaceKnownVersions = (content, oxcVersion, resolverVersion) =>
  content
    .replaceAll(oxcStackConfig.currentOxcVersion, oxcVersion)
    .replaceAll(oxcStackConfig.currentResolverVersion, resolverVersion);

const updateConfig = (content, oxcVersion, resolverVersion) =>
  content
    .replace(
      /currentOxcVersion:\s*"[^"]+"/u,
      `currentOxcVersion: "${oxcVersion}"`,
    )
    .replace(
      /currentResolverVersion:\s*"[^"]+"/u,
      `currentResolverVersion: "${resolverVersion}"`,
    );

const updateLockfiles = async (execFile, oxcVersion, resolverVersion) => {
  await execFile("pnpm", ["install", "--lockfile-only"]);
  await execFile("cargo", ["update", "-p", "oxc_resolver", "--precise", resolverVersion]);
  for (const crate of oxcStackConfig.oxcCrates) {
    await execFile("cargo", ["update", "-p", crate, "--precise", oxcVersion]);
  }
};

const formatResult = ({ dryRun, oxcVersion, resolverVersion, changedFiles }) => {
  const mode = dryRun ? "Dry run" : "Updated";
  const files = changedFiles.length === 0 ? "No file edits needed." : `Files: ${changedFiles.join(", ")}`;
  return `${mode}: OXC ${oxcVersion}, oxc_resolver ${resolverVersion}\n${files}\n`;
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
  --oxc <version>       Target OXC monorepo version. Defaults to latest oxc-parser.
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
