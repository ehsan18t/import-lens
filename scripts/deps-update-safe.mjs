#!/usr/bin/env node

import { execFile as execFileCallback } from "node:child_process";
import { readFile as fsReadFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { compilerStackConfig } from "./compiler-stack.config.mjs";
import {
  computeCompilerStackFingerprint,
  FINGERPRINT_PATH,
  formatFingerprint,
} from "./compiler-stack-fingerprint.mjs";

const execFilePromise = promisify(execFileCallback);

// The set of crates the restore must pin back, and the exact version each must
// return to, read straight from the committed fingerprint -- the ONLY complete
// record of the coordinated closure. The direct crates (rolldown, its support
// siblings, the retained OXC crates, oxc_resolver, the glob matcher) are a
// minority of it: rolldown pins its ~40 workspace siblings with caret ranges,
// so a range-respecting `cargo update` drifts them within their carets. Pinning
// only the direct crates leaves those siblings drifted and the recompute
// diverges, which is exactly the failure this function exists to prevent.
//
// registry only: path/git sources cannot drift on a `cargo update` and cannot be
// pinned with `--precise` anyway. The fingerprint records all 53 as registry, so
// this filter is a guard, not a live subtraction.
export const deriveRestorePins = (fingerprintText) =>
  JSON.parse(fingerprintText)
    .packages.filter((pkg) => typeof pkg.source === "string" && pkg.source !== "path")
    .map((pkg) => [pkg.name, pkg.version]);

// Range-respecting refresh of everything OUTSIDE the compiler stack. `pnpm
// update` stays within package.json ranges and `cargo update` within
// Cargo.toml's, then the coordinated Rolldown/OXC/resolver packages are
// restored to the recorded set. Rolldown's own caret ranges allow a general
// update to move its workspace crates, so success is only reported after the
// recomputed fingerprint matches the committed one.
export const runSafeUpdate = async ({
  execFile = execFilePromise,
  readFile = fsReadFile,
  rootDir = process.cwd(),
  platform = process.platform,
  stdout = undefined,
} = {}) => {
  // On Windows `pnpm` resolves to `pnpm.CMD`, which execFile (CreateProcess)
  // cannot launch directly; run it through the shell, mirroring the packaging
  // scripts. cargo is a real executable and stays on execFile.
  if (platform === "win32") {
    await execFile("pnpm update", { shell: true, cwd: rootDir });
  } else {
    await execFile("pnpm", ["update"], { cwd: rootDir });
  }
  await execFile("cargo", ["update"], { cwd: rootDir });

  // One read of the committed fingerprint serves both roles: it names every
  // crate to pin back (below) and is the target the recompute must match (end).
  const committed = await readFile(path.join(rootDir, FINGERPRINT_PATH), "utf8");
  for (const [crate, version] of deriveRestorePins(committed)) {
    await execFile("cargo", ["update", "-p", crate, "--precise", version], { cwd: rootDir });
  }

  const recomputed = formatFingerprint(
    await computeCompilerStackFingerprint({ execFile, rootDir }),
  );
  if (recomputed !== committed) {
    throw new Error(
      "deps:update:safe could not restore the recorded compiler stack; the resolved " +
        "Rolldown/OXC graph no longer matches scripts/compiler-stack.fingerprint.json. " +
        "Run pnpm deps:update:compiler to move the stack deliberately.",
    );
  }

  stdout?.write(
    `Safe update complete: compiler stack restored (rolldown ${compilerStackConfig.currentRolldownVersion}, ` +
      `OXC ${compilerStackConfig.currentOxcVersion}, oxc_resolver ${compilerStackConfig.currentResolverVersion}).\n`,
  );
};

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    await runSafeUpdate({ stdout: process.stdout });
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
