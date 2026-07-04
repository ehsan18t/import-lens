#!/usr/bin/env node

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

// Resolve the effective release version: a non-empty, explicitly requested
// version wins; otherwise fall back to the version committed in package.json.
export const resolveVersion = (requested, manifestVersion) => {
  const trimmed = (requested ?? "").trim();
  return trimmed || manifestVersion;
};

const main = () => {
  const manifest = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
  const version = resolveVersion(process.argv[2], manifest.version);

  if (!version) {
    console.error("Could not resolve a version: no input was given and package.json has no version.");
    process.exit(1);
  }

  console.log(version);
};

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main();
}
