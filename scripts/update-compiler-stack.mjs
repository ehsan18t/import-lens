#!/usr/bin/env node

import { execFile as execFileCallback } from "node:child_process";
import {
  mkdtemp as fsMkdtemp,
  readFile as fsReadFile,
  rm as fsRm,
  writeFile as fsWriteFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { compilerStackConfig } from "./compiler-stack.config.mjs";
import {
  computeCompilerStackFingerprint,
  formatFingerprint,
} from "./compiler-stack-fingerprint.mjs";
import {
  formatCompilerUpdateResult,
  latestCrateVersion,
  replaceKnownVersions,
  rolldownFamilyCrates,
  updateCargoToml,
  updateConfig,
  updateManifest,
  validateAvailableVersions,
  validateCurrentStack,
  validateVersion,
} from "./compiler-stack-helpers.mjs";

const execFilePromise = promisify(execFileCallback);

// Tests never carry stack version literals -- they derive from
// compiler-stack.config.mjs, which is the sole source of truth.
const defaultPaths = {
  cargoToml: "daemon/Cargo.toml",
  manifest: "package.json",
  srs: "docs/ImportLens-SRS.md",
  config: "scripts/compiler-stack.config.mjs",
  fingerprint: "scripts/compiler-stack.fingerprint.json",
};

export const parseUpdateArgs = (argv) => {
  const parsed = {
    dryRun: false,
    rolldownVersion: undefined,
    oxcVersion: undefined,
    resolverVersion: undefined,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    // npm requires `--` to forward flags to the script; pnpm passes it through
    // verbatim. Accept it from either so one documented command works in both.
    if (arg === "--") {
      continue;
    }
    if (arg === "--dry-run") {
      parsed.dryRun = true;
      continue;
    }
    if (arg === "--rolldown") {
      parsed.rolldownVersion = valueAfter(argv, index, arg);
      index += 1;
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

export const updateCompilerStack = async ({
  rootDir = process.cwd(),
  paths = defaultPaths,
  dryRun = false,
  rolldownVersion,
  oxcVersion,
  resolverVersion,
  fetchJson = defaultFetchJson,
  readFile = fsReadFile,
  writeFile = fsWriteFile,
  execFile = execFilePromise,
  mkdtemp = fsMkdtemp,
  rm = fsRm,
  probeWriteFile = fsWriteFile,
  platform = process.platform,
  stdout = undefined,
} = {}) => {
  const absolutePaths = Object.fromEntries(
    Object.entries({ ...defaultPaths, ...paths }).map(([key, relativePath]) => [
      key,
      path.join(rootDir, relativePath),
    ]),
  );

  const files = await readFiles(absolutePaths, readFile);
  validateCurrentStack(files.cargoToml);

  const targetRolldownVersion =
    rolldownVersion ?? (await latestCrateVersion(fetchJson, compilerStackConfig.rolldownCrate));
  validateVersion("rolldown", targetRolldownVersion);
  if (oxcVersion !== undefined) {
    validateVersion("OXC", oxcVersion);
  }
  if (resolverVersion !== undefined) {
    validateVersion("oxc_resolver", resolverVersion);
  }

  // Cargo is the compatibility authority: resolve the requested stack in a
  // throwaway manifest outside the repository before touching a tracked file.
  const probe = await resolveProbeStack({
    execFile,
    mkdtemp,
    rm,
    probeWriteFile,
    rolldownVersion: targetRolldownVersion,
    oxcVersion,
    resolverVersion,
  });

  const targetOxcVersion = probe.oxcVersion;
  const targetResolverVersion = probe.resolverVersion;
  validateVersion("OXC", targetOxcVersion);
  validateVersion("oxc_resolver", targetResolverVersion);
  await validateAvailableVersions(fetchJson, {
    rolldownVersion: targetRolldownVersion,
    oxcVersion: targetOxcVersion,
    resolverVersion: targetResolverVersion,
  });

  const targetVersions = {
    rolldownVersion: targetRolldownVersion,
    oxcVersion: targetOxcVersion,
    resolverVersion: targetResolverVersion,
  };

  const nextFiles = {
    cargoToml: updateCargoToml(files.cargoToml, targetVersions),
    manifest: updateManifest(files.manifestJson),
    srs: replaceKnownVersions(files.srs, targetVersions),
    config: updateConfig(files.config, targetVersions),
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
    await updateLockfiles(execFile, targetVersions, platform, rootDir);

    const fingerprint = formatFingerprint(
      await computeCompilerStackFingerprint({ execFile, rootDir }),
    );
    if (fingerprint !== files.fingerprint) {
      await writeFile(absolutePaths.fingerprint, fingerprint, "utf8");
      changedFiles.push(paths.fingerprint ?? defaultPaths.fingerprint);
    }
  }

  const result = { ...targetVersions, changedFiles, dryRun };

  stdout?.write(formatCompilerUpdateResult(result));
  return result;
};

// Resolves rolldown plus any explicit overrides in a temporary Cargo package
// and reads the versions Cargo selected. An unsatisfiable combination fails
// here, before any tracked file changes.
const resolveProbeStack = async ({
  execFile,
  mkdtemp,
  rm,
  probeWriteFile,
  rolldownVersion,
  oxcVersion,
  resolverVersion,
}) => {
  const probeDir = await mkdtemp(path.join(os.tmpdir(), "importlens-compiler-stack-"));
  try {
    const overrides = [];
    if (oxcVersion !== undefined) {
      overrides.push(
        ...compilerStackConfig.oxcCrates.map((crate) => `${crate} = "=${oxcVersion}"`),
      );
    }
    if (resolverVersion !== undefined) {
      overrides.push(`oxc_resolver = "=${resolverVersion}"`);
    }

    await probeWriteFile(path.join(probeDir, "lib.rs"), "", "utf8");
    await probeWriteFile(
      path.join(probeDir, "Cargo.toml"),
      [
        "[package]",
        'name = "compiler-stack-probe"',
        'version = "0.0.0"',
        'edition = "2024"',
        "",
        "[lib]",
        'path = "lib.rs"',
        "",
        "[dependencies]",
        // Mirror the real manifest's constraint shape: the support crates
        // are pinned at rolldown's version, so a release where they did not
        // ship at that version must fail HERE, before any tracked edit.
        ...rolldownFamilyCrates().map((crate) => `${crate} = "=${rolldownVersion}"`),
        ...overrides,
        "",
      ].join("\n"),
      "utf8",
    );

    let metadata;
    try {
      const { stdout } = await execFile(
        "cargo",
        ["metadata", "--format-version", "1", "--manifest-path", path.join(probeDir, "Cargo.toml")],
        { maxBuffer: 256 * 1024 * 1024 },
      );
      metadata = JSON.parse(stdout);
    } catch (error) {
      const detail = error?.stderr ? `\n${String(error.stderr).trim()}` : ` ${error.message}`;
      throw new Error(`Unsatisfiable compiler stack:${detail}`);
    }

    assertSingleCoordinatedVersions(metadata);

    const resolved = {
      rolldownVersion: resolvedVersion(metadata, compilerStackConfig.rolldownCrate),
      oxcVersion: resolvedVersion(metadata, "oxc_parser"),
      resolverVersion: resolvedVersion(metadata, "oxc_resolver"),
    };

    // The overrides were exact constraints in the probe manifest, so a
    // disagreement here is a resolution bug, not a user error.
    assertDerivedMatches("rolldown", resolved.rolldownVersion, rolldownVersion);
    assertDerivedMatches("OXC", resolved.oxcVersion, oxcVersion);
    assertDerivedMatches("oxc_resolver", resolved.resolverVersion, resolverVersion);

    return resolved;
  } finally {
    await rm(probeDir, { recursive: true, force: true });
  }
};

// Cargo does not fail on semver-incompatible duplicates -- it resolves BOTH
// copies. An explicit override fighting rolldown's own requirement therefore
// "succeeds" as a split stack; reject that here, before any tracked edit.
const assertSingleCoordinatedVersions = (metadata) => {
  const coordinated = new Set([
    ...rolldownFamilyCrates(),
    "oxc",
    "oxc_resolver",
    ...compilerStackConfig.oxcCrates,
  ]);
  const versionsByName = new Map();
  for (const pkg of metadata.packages ?? []) {
    if (!coordinated.has(pkg.name)) {
      continue;
    }
    const versions = versionsByName.get(pkg.name) ?? new Set();
    versions.add(pkg.version);
    versionsByName.set(pkg.name, versions);
  }
  const split = [...versionsByName].filter(([, versions]) => versions.size > 1);
  if (split.length > 0) {
    const detail = split
      .map(
        ([name, versions]) =>
          `${name} resolves to multiple versions (${[...versions].sort().join(", ")})`,
      )
      .join("; ");
    throw new Error(`Unsatisfiable compiler stack: ${detail}`);
  }
};

const resolvedVersion = (metadata, crateName) => {
  const version = metadata.packages?.find((pkg) => pkg.name === crateName)?.version;
  if (!version) {
    throw new Error(`${crateName} is not present in the probe-resolved graph`);
  }
  return version;
};

const assertDerivedMatches = (label, derived, requested) => {
  if (requested !== undefined && derived !== requested) {
    throw new Error(`Cargo resolved ${label} ${derived}, not the requested ${requested}`);
  }
};

const readFiles = async (absolutePaths, readFile) => {
  const manifest = await readText(readFile, absolutePaths.manifest);
  return {
    cargoToml: await readText(readFile, absolutePaths.cargoToml),
    manifest,
    manifestJson: JSON.parse(manifest),
    srs: await readText(readFile, absolutePaths.srs),
    config: await readText(readFile, absolutePaths.config),
    fingerprint: await readText(readFile, absolutePaths.fingerprint).catch(() => ""),
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

const updateLockfiles = async (
  execFile,
  { rolldownVersion, oxcVersion, resolverVersion },
  platform,
  rootDir,
) => {
  // Lockfile commands must run in the same repo the tracked-file edits and
  // fingerprint recompute target, not whatever cwd the caller happens to have.
  // On Windows `pnpm` resolves to `pnpm.CMD`, which execFile (CreateProcess)
  // cannot launch directly; run it through the shell, mirroring the packaging
  // scripts. cargo is a real executable and stays on execFile.
  if (platform === "win32") {
    await execFile("pnpm install --lockfile-only", { shell: true, cwd: rootDir });
  } else {
    await execFile("pnpm", ["install", "--lockfile-only"], { cwd: rootDir });
  }
  for (const crate of rolldownFamilyCrates()) {
    await execFile("cargo", ["update", "-p", crate, "--precise", rolldownVersion], {
      cwd: rootDir,
    });
  }
  await execFile("cargo", ["update", "-p", "oxc_resolver", "--precise", resolverVersion], {
    cwd: rootDir,
  });
  for (const crate of compilerStackConfig.oxcCrates) {
    await execFile("cargo", ["update", "-p", crate, "--precise", oxcVersion], { cwd: rootDir });
  }
};

const defaultFetchJson = async (url) => {
  const response = await fetch(url, {
    headers: {
      "User-Agent": "import-lens-compiler-stack-updater",
      Accept: "application/json",
    },
  });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
};

const printHelp = () => {
  process.stdout.write(`Usage: pnpm deps:update:compiler [options]

Options:
  --rolldown <version>  Target rolldown version. Defaults to the latest stable crate.
  --oxc <version>       Explicit OXC monorepo version. Defaults to the version Cargo
                        resolves for the requested rolldown release.
  --resolver <version>  Explicit oxc_resolver version. Defaults to the version Cargo
                        resolves for the requested rolldown release.
  --dry-run            Resolve and validate without writing any file or lockfile.
  -h, --help           Show this help.
`);
};

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const args = parseUpdateArgs(process.argv.slice(2));
    if (args.help) {
      printHelp();
    } else {
      await updateCompilerStack({ ...args, stdout: process.stdout });
    }
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
