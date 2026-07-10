#!/usr/bin/env node

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { resolveVersion } from "./resolve-version.mjs";
import { crossCompilerForTarget, platformTargets, vsixNameForTarget } from "./targets.mjs";

// Shell and CI cannot import targets.mjs. Rather than let them keep their own
// copies of the target list, they read it from here.

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const [mode, requestedVersion] = process.argv.slice(2);

if (mode === "--build-plan") {
  for (const target of platformTargets) {
    console.log(`${target} --${crossCompilerForTarget(target)}`);
  }
} else if (mode === "--vsix") {
  const manifest = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
  const version = resolveVersion(requestedVersion, manifest.version);

  // Mirrors resolve-version.mjs: an empty version would emit artifact names the
  // release gate then "verifies" against, so fail loudly instead.
  if (!version) {
    console.error(
      "Could not resolve a version: no input was given and package.json has no version.",
    );
    process.exit(1);
  }

  for (const target of platformTargets) {
    console.log(vsixNameForTarget({ ...manifest, version }, target));
  }
} else {
  console.error("Usage: node scripts/print-targets.mjs <--build-plan | --vsix [version]>");
  process.exit(1);
}
