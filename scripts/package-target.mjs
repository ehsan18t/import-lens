#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { targetInfo, vsixNameForTarget } from "./targets.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const useZigbuild = args.includes("--zigbuild");
const useXwin = args.includes("--xwin");
const target = args.find((arg) => !arg.startsWith("--"));

const fail = (message) => {
  console.error(message);
  process.exit(1);
};

const run = (command, commandArgs) => {
  const needsShell = process.platform === "win32" && command === "pnpm";
  const result = needsShell
    ? spawnSync(`${command} ${commandArgs.join(" ")}`, {
        cwd: repoRoot,
        shell: true,
        stdio: "inherit",
      })
    : spawnSync(command, commandArgs, { cwd: repoRoot, stdio: "inherit" });

  if (result.error) {
    fail(result.error.message);
  }

  if (result.status !== 0) {
    fail(`${command} ${commandArgs.join(" ")} failed with exit code ${result.status ?? "unknown"}`);
  }
};

if (!target) {
  fail("Usage: node scripts/package-target.mjs <target> [--zigbuild | --xwin]");
}

if (useZigbuild && useXwin) {
  fail("Choose one cross-compiler: pass --zigbuild or --xwin, not both.");
}

const crossCompileFlags = useXwin ? ["--xwin"] : useZigbuild ? ["--zigbuild"] : [];

targetInfo(target);

const manifest = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
const vsixName = vsixNameForTarget(manifest, target);

if (manifest.icon && !existsSync(path.join(repoRoot, manifest.icon))) {
  fail(`Extension icon is declared at ${manifest.icon}, but the file does not exist.`);
}

run(process.execPath, ["scripts/build-daemon.mjs", target, ...crossCompileFlags]);
run(process.execPath, ["scripts/copy-daemon.mjs", target]);
run(process.execPath, ["scripts/generate-daemon-hashes.mjs", target]);
run("pnpm", ["build"]);
run(process.execPath, ["scripts/package-vsix.mjs", target]);
run("pnpm", ["assert:vsix-size", vsixName]);
