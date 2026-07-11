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

  const pins = [
    [compilerStackConfig.rolldownCrate, compilerStackConfig.currentRolldownVersion],
    ["oxc_resolver", compilerStackConfig.currentResolverVersion],
    ...compilerStackConfig.oxcCrates.map((crate) => [crate, compilerStackConfig.currentOxcVersion]),
  ];
  for (const [crate, version] of pins) {
    await execFile("cargo", ["update", "-p", crate, "--precise", version], { cwd: rootDir });
  }

  const recomputed = formatFingerprint(
    await computeCompilerStackFingerprint({ execFile, rootDir }),
  );
  const committed = await readFile(path.join(rootDir, FINGERPRINT_PATH), "utf8");
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
