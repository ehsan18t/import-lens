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

const CARGO_LOCK_PATH = "Cargo.lock";

// A pin cannot land while a caret-drifted DEPENDENT still requires the newer
// version, so the restore needs more than one pass: `rolldown_sourcemap` drifting
// to 1.2.0 requires `oxc ^0.140.0`, which makes `oxc` unpinnable until that
// sibling is itself restored -- and the fingerprint's name order reaches `oxc`
// long before any `rolldown_*`. Each sweep re-reads the lock, so restoring a
// dependent (which makes cargo drop the copies it dragged in outright) is seen by
// the next one. Depth of the drifted chain is 2 today; the bound is slack, and a
// sweep that pins nothing exits early because the lock is then provably unchanged.
const MAX_RESTORE_SWEEPS = 8;

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

// Locked versions per crate name, straight from Cargo.lock. A name maps to more
// than one version exactly when a drifted dependent pulled a second copy in --
// the state that makes a bare `-p <name>` spec ambiguous and abort the restore.
export const lockedVersionsByName = (lockText) => {
  const versions = new Map();
  for (const block of lockText.split(/\r?\n\[\[package\]\]\r?\n/u).slice(1)) {
    const name = /^name = "(?<name>[^"]+)"$/mu.exec(block)?.groups?.name;
    const version = /^version = "(?<version>[^"]+)"$/mu.exec(block)?.groups?.version;
    if (name !== undefined && version !== undefined) {
      versions.set(name, [...(versions.get(name) ?? []), version]);
    }
  }
  return versions;
};

// Every locked copy that is not already at its recorded version, as
// [crate, lockedVersion, targetVersion]. A crate at the recorded version yields
// nothing, so a converged lock produces an empty sweep and the restore stops.
export const outstandingPins = (lockText, pins) => {
  const locked = lockedVersionsByName(lockText);
  return pins.flatMap(([crate, target]) =>
    (locked.get(crate) ?? [])
      .filter((version) => version !== target)
      .map((version) => [crate, version, target]),
  );
};

const describeExecError = (error) =>
  String(error?.stderr ?? "").trim() || (error instanceof Error ? error.message : String(error));

// Pins every drifted copy back to its recorded version, sweeping until the lock
// converges. Individual failures are collected rather than thrown: mid-sweep a
// crate can legitimately refuse to move (a dependent still requires the drifted
// version) or its spec can go stale (an earlier pin already made cargo drop that
// copy), and both resolve on a later sweep. Only the caller's fingerprint
// comparison decides success -- these strings exist to explain a failure, not to
// cause one.
const restoreCoordinatedStack = async ({ execFile, readFile, rootDir, pins }) => {
  const lockPath = path.join(rootDir, CARGO_LOCK_PATH);
  let refusals = [];

  for (let sweep = 0; sweep < MAX_RESTORE_SWEEPS; sweep += 1) {
    const outstanding = outstandingPins(await readFile(lockPath, "utf8"), pins);
    if (outstanding.length === 0) {
      return [];
    }

    refusals = [];
    let pinned = 0;
    for (const [crate, locked, target] of outstanding) {
      try {
        // `<name>@<version>`, never a bare name: once a drifted dependent has
        // pulled a second copy in, cargo rejects the bare spec as ambiguous.
        await execFile("cargo", ["update", "-p", `${crate}@${locked}`, "--precise", target], {
          cwd: rootDir,
        });
        pinned += 1;
      } catch (error) {
        refusals.push(`${crate} ${locked} -> ${target}: ${describeExecError(error)}`);
      }
    }

    // Nothing moved, so the lock is byte-identical and the next sweep would
    // compute the same set and fail the same way. Stop and let the caller report.
    if (pinned === 0) {
      break;
    }
  }

  return refusals;
};

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
  const refusals = await restoreCoordinatedStack({
    execFile,
    readFile,
    rootDir,
    pins: deriveRestorePins(committed),
  });

  const recomputed = formatFingerprint(
    await computeCompilerStackFingerprint({ execFile, rootDir }),
  );
  if (recomputed !== committed) {
    throw new Error(
      "deps:update:safe could not restore the recorded compiler stack; the resolved " +
        "Rolldown/OXC graph no longer matches scripts/compiler-stack.fingerprint.json. " +
        "Run pnpm deps:update:compiler to move the stack deliberately." +
        // Without these the failure names only the symptom; the refusal cargo
        // gave for each crate is what says WHICH pin could not land and why.
        (refusals.length > 0 ? `\nUnrestored pins:\n  ${refusals.join("\n  ")}` : ""),
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
