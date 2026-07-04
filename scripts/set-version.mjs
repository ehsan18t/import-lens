#!/usr/bin/env node

import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

// Rewrite the manifest's "version" field. Key order is preserved (JSON.parse
// keeps insertion order and "version" is overwritten in place), and the write
// is skipped when the value already matches so an empty-input build causes no
// churn. Returns { changed, content } so the caller can avoid a no-op write.
export const applyVersion = (manifestSource, version) => {
  const manifest = JSON.parse(manifestSource);

  if (manifest.version === version) {
    return { changed: false, content: manifestSource };
  }

  manifest.version = version;
  return { changed: true, content: `${JSON.stringify(manifest, null, 2)}\n` };
};

const main = () => {
  const version = (process.argv[2] ?? "").trim();

  if (!version) {
    console.error("Usage: node scripts/set-version.mjs <version>");
    process.exit(1);
  }

  const manifestPath = path.join(repoRoot, "package.json");
  const { changed, content } = applyVersion(readFileSync(manifestPath, "utf8"), version);

  if (changed) {
    writeFileSync(manifestPath, content);
    console.log(`Set package.json version to ${version}.`);
  } else {
    console.log(`package.json version already ${version}; left unchanged.`);
  }
};

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  main();
}
