#!/usr/bin/env node

import { readdirSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const maxBytes = 20 * 1024 * 1024;
const inputs = process.argv.slice(2);
const vsixFiles = inputs.length > 0
  ? inputs
  : readdirSync(repoRoot).filter((entry) => entry.endsWith(".vsix"));

if (vsixFiles.length === 0) {
  console.error("No VSIX files were provided or found in the repository root.");
  process.exit(1);
}

let failed = false;

for (const file of vsixFiles) {
  const absolutePath = path.resolve(repoRoot, file);
  const size = statSync(absolutePath).size;
  const sizeMb = (size / (1024 * 1024)).toFixed(2);

  if (size > maxBytes) {
    console.error(`${path.basename(file)} exceeds 20 MB (${sizeMb} MB).`);
    failed = true;
    continue;
  }

  console.log(`${path.basename(file)} is ${sizeMb} MB.`);
}

if (failed) {
  process.exit(1);
}
