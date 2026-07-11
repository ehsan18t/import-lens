#!/usr/bin/env node

// Installs the pinned accuracy fixtures into a stable workspace for the
// candidate qualification suites (daemon/tests/candidate_packages.rs and
// candidate_performance.rs). Network access happens HERE, as an explicit
// setup step — the Rust tests themselves never touch the network.
//
// Usage: node scripts/prepare-candidate-fixtures.mjs [target-dir]
// Prints the workspace directory; export it as
// IMPORT_LENS_FIXTURES_WORKSPACE before running the ignored test suites.

import { spawn } from "node:child_process";
import { copyFile, mkdir, writeFile } from "node:fs/promises";
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

const target = process.argv[2] ?? path.join(os.tmpdir(), "import-lens-candidate-fixtures");
await mkdir(target, { recursive: true });
await copyFile(path.join(fixturesDir, "package.json"), path.join(target, "package.json"));
await copyFile(path.join(fixturesDir, "pnpm-lock.yaml"), path.join(target, "pnpm-lock.yaml"));
await writeFile(path.join(target, ".npmrc"), npmrc, "utf8");
await run(
  "pnpm",
  ["install", "--frozen-lockfile", "--ignore-workspace", "--ignore-scripts", "--prefer-offline"],
  target,
);
process.stdout.write(`${target}\n`);
