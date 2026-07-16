#!/usr/bin/env node

// Installs the pinned accuracy fixtures into a stable workspace for the
// candidate qualification suites (daemon/tests/candidate_packages.rs and
// candidate_performance.rs). Network access happens HERE, as an explicit
// setup step — the Rust tests themselves never touch the network.
//
// Preparation is reproducible: node_modules is recreated from scratch on every
// run, and the install is verified against the fixture manifest before the path
// is printed. A workspace that does not contain every pinned package's entry
// files is reported as a failure instead of being handed to the suites.
//
// Usage: node scripts/prepare-candidate-fixtures.mjs [target-dir]
// Prints the workspace directory; export it as
// IMPORT_LENS_FIXTURES_WORKSPACE before running the ignored test suites.

import { spawn } from "node:child_process";
import { copyFile, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const fixturesDir = fileURLToPath(new URL("accuracy-fixtures/", import.meta.url));

// Keep in step with accuracy-compare.mjs `fixtureNpmrc`. For the two
// lockfile-recorded settings (auto-install-peers, exclude-links-from-
// lockfile) drift fails loudly: pnpm refuses a --frozen-lockfile install
// when an effective setting disagrees with the lockfile
// (ERR_PNPM_LOCKFILE_CONFIG_MISMATCH). node-linker is NOT lockfile-validated
// — drifting it would silently change the node_modules shape, so treat that
// line as manually coupled to accuracy-compare.mjs.
const npmrc = [
  "node-linker=hoisted",
  "auto-install-peers=true",
  "exclude-links-from-lockfile=false",
  "",
].join("\n");

const run = (command, args, cwd) =>
  new Promise((resolve, reject) => {
    // On Windows `pnpm` resolves to `pnpm.CMD`, which CreateProcess cannot
    // launch directly; run it through the shell, mirroring accuracy-compare.
    const child =
      process.platform === "win32"
        ? spawn(`${command} ${args.join(" ")}`, { cwd, shell: true, stdio: "inherit" })
        : spawn(command, args, { cwd, stdio: "inherit" });
    child.on("error", reject);
    child.on("exit", (code) =>
      code === 0 ? resolve() : reject(new Error(`${command} exited with code ${code}`)),
    );
  });

const readJson = async (file) => JSON.parse(await readFile(file, "utf8"));

const isFile = async (file) => {
  try {
    return (await stat(file)).isFile();
  } catch {
    return false;
  }
};

// Every file a package declares as its root (".") entry: the condition leaves
// of `exports["."]` plus the legacy `main`/`module` fields. Wildcard subpaths
// are patterns, not files, so they are skipped; `types` leaves are skipped
// because a missing .d.ts says nothing about the code the daemon bundles.
const rootEntryFiles = (manifest) => {
  const files = new Set();
  const collect = (node) => {
    if (typeof node === "string") {
      // `exports` writes "./index.js" where `main` writes "index.js"; the same
      // file must not be reported twice.
      if (!node.includes("*")) {
        files.add(node.replace(/^\.\//u, ""));
      }
      return;
    }
    if (Array.isArray(node)) {
      for (const value of node) {
        collect(value);
      }
      return;
    }
    if (node === null || typeof node !== "object") {
      return;
    }
    for (const [condition, value] of Object.entries(node)) {
      if (condition !== "types" && condition !== "typings") {
        collect(value);
      }
    }
  };

  const { exports } = manifest;
  if (typeof exports === "string" || Array.isArray(exports)) {
    collect(exports);
  } else if (exports !== null && typeof exports === "object") {
    // A subpath map keys every entry with "."; anything else is a bare
    // condition map, which describes "." itself.
    const subpathMap = Object.keys(exports).some((key) => key.startsWith("."));
    collect(subpathMap ? exports["."] : exports);
  }
  collect(manifest.main);
  collect(manifest.module);
  // Node's implicit default when a package declares no entry at all.
  return files.size > 0 ? [...files] : ["index.js"];
};

// pnpm considers an install current from its own bookkeeping, not from the
// files on disk: an interrupted or corrupted install leaves node_modules in a
// state a --frozen-lockfile run happily reports as up to date while package
// files are missing. Recreating node_modules removes that bookkeeping, and
// this check refuses to hand callers a workspace that is still incomplete.
const missingPackages = async (workspace) => {
  const { dependencies } = await readJson(path.join(fixturesDir, "package.json"));
  const problems = [];
  for (const [name, spec] of Object.entries(dependencies ?? {})) {
    const packageDir = path.join(workspace, "node_modules", ...name.split("/"));
    const manifestPath = path.join(packageDir, "package.json");
    if (!(await isFile(manifestPath))) {
      problems.push(`${name}@${spec}: node_modules/${name}/package.json is missing`);
      continue;
    }
    const manifest = await readJson(manifestPath);
    if (/^\d+\.\d+\.\d+/u.test(spec) && manifest.version !== spec) {
      problems.push(`${name}@${spec}: installed version is ${manifest.version}`);
    }
    for (const entry of rootEntryFiles(manifest)) {
      const entryPath = path.join(packageDir, entry);
      if (!(await isFile(entryPath))) {
        problems.push(`${name}@${spec}: entry file ${entry} is missing`);
      }
    }
  }
  return problems;
};

const target = process.argv[2] ?? path.join(os.tmpdir(), "import-lens-candidate-fixtures");
await rm(path.join(target, "node_modules"), { recursive: true, force: true });
await mkdir(target, { recursive: true });
await copyFile(path.join(fixturesDir, "package.json"), path.join(target, "package.json"));
await copyFile(path.join(fixturesDir, "pnpm-lock.yaml"), path.join(target, "pnpm-lock.yaml"));
await writeFile(path.join(target, ".npmrc"), npmrc, "utf8");
await run(
  "pnpm",
  ["install", "--frozen-lockfile", "--ignore-workspace", "--ignore-scripts", "--prefer-offline"],
  target,
);

const problems = await missingPackages(target);
if (problems.length > 0) {
  process.stderr.write(
    [
      `pnpm reported a successful install, but ${target} is incomplete:`,
      ...problems.map((problem) => `  - ${problem}`),
      "Refusing to print the workspace path: the qualification suites would fail misleadingly.",
      "",
    ].join("\n"),
  );
  process.exit(1);
}

// Callers read this last line into IMPORT_LENS_FIXTURES_WORKSPACE; it is only
// ever printed for a workspace whose packages were just verified on disk.
process.stdout.write(`${target}\n`);
