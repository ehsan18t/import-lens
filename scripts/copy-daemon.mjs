#!/usr/bin/env node

import { cpSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { artifactPathForTarget, relativeDaemonPath, targetInfo } from "./targets.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const target = process.argv[2];

if (!target) {
  console.error("Usage: node scripts/copy-daemon.mjs <target>");
  process.exit(1);
}

targetInfo(target);
const source = artifactPathForTarget(repoRoot, target);

if (!existsSync(source)) {
  console.error(`Daemon binary not found: ${source}`);
  console.error(`Run node scripts/build-daemon.mjs ${target} first.`);
  process.exit(1);
}

const destination = path.join(repoRoot, relativeDaemonPath(target));
const targetDir = path.dirname(destination);

mkdirSync(targetDir, { recursive: true });
cpSync(source, destination, { force: true });
console.log(`Copied ${source} to ${destination}`);
