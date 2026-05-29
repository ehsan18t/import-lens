#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  cargoBuildArgsForTarget,
  cargoZigbuildArgsForTarget,
  currentPlatformTarget,
  targetInfo,
} from "./targets.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const useZigbuild = args.includes("--zigbuild");
const platformTarget = args.find((arg) => !arg.startsWith("--")) ?? currentPlatformTarget();

if (!platformTarget) {
  console.error("Usage: node scripts/build-daemon.mjs <target> [--zigbuild]");
  process.exit(1);
}

const info = targetInfo(platformTarget);
const cargoArgs = useZigbuild
  ? cargoZigbuildArgsForTarget(platformTarget)
  : cargoBuildArgsForTarget(platformTarget);

console.log(`Building daemon for ${platformTarget} (${info.rustTarget})...`);
const result = spawnSync("cargo", cargoArgs, {
  cwd: repoRoot,
  stdio: "inherit",
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 1);
