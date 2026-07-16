import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { FINGERPRINT_PATH } from "../compiler-stack-fingerprint.mjs";
import { deriveRestorePins, runSafeUpdate } from "../deps-update-safe.mjs";

const rootDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

const readCommittedFingerprint = () => readFile(path.join(rootDir, FINGERPRINT_PATH), "utf8");

// A `cargo metadata` graph whose fingerprint recomputes to exactly the given
// committed text: same packages, and a root `rolldown` that reaches every other
// coordinated crate so the recompute's BFS visits the whole closure -- which is
// how the real graph pulls it in. runSafeUpdate then sees recomputed === committed
// and reports success, letting the wiring test exercise the happy path.
const metadataMatching = (fingerprintText) => {
  const withIds = JSON.parse(fingerprintText).packages.map((pkg, index) => ({
    ...pkg,
    id: `id:${pkg.name}:${index}`,
  }));
  const root = withIds.find((pkg) => pkg.name === "rolldown");
  return {
    packages: withIds,
    resolve: {
      nodes: withIds.map((pkg) => ({
        id: pkg.id,
        dependencies:
          pkg.id === root.id ? withIds.filter((o) => o.id !== root.id).map((o) => o.id) : [],
      })),
    },
  };
};

// Records every exec as [command, args]; answers `cargo metadata` with the
// supplied graph and everything else (pnpm update, cargo update, the pins) with
// empty stdout. Handles both call shapes: (cmd, options) and (cmd, args, options).
const recordingExec = (calls, metadata) => async (command, argsOrOptions) => {
  const args = Array.isArray(argsOrOptions) ? argsOrOptions : [];
  calls.push([command, args]);
  if (command === "cargo" && args[0] === "metadata") {
    return { stdout: JSON.stringify(metadata) };
  }
  return { stdout: "" };
};

// DRIFT: the expectation is DERIVED from the committed fingerprint, not typed
// out. `deps:update:safe` restores the coordinated compiler stack by pinning it
// back after a range-respecting `cargo update`; if it fails to pin even one
// package the fingerprint records, the recompute diverges and the command
// throws for the very case it exists to handle. Add a crate to the fingerprint
// (or break the derivation) and this goes red.
test("the restore derives a pin for every package the fingerprint records", async () => {
  const text = await readCommittedFingerprint();
  const expected = JSON.parse(text).packages.map((pkg) => pkg.name);
  const pinned = deriveRestorePins(text).map(([name]) => name);
  const missing = expected.filter((name) => !pinned.includes(name));
  assert.deepEqual(missing, [], "deps:update:safe cannot restore a package it never pins");
});

test("deriveRestorePins yields a [name, version] pin per registry package and drops path deps", () => {
  const pins = deriveRestorePins(
    JSON.stringify({
      packages: [
        { name: "rolldown", version: "1.1.5", source: "registry+https://example/index" },
        { name: "oxc_parser", version: "0.139.0", source: "registry+https://example/index" },
        // A path source cannot drift on `cargo update` and cannot take --precise,
        // so it must not become a pin.
        { name: "rolldown_local", version: "1.1.5", source: null },
      ],
    }),
  );

  assert.deepEqual(pins, [
    ["rolldown", "1.1.5"],
    ["oxc_parser", "0.139.0"],
  ]);
});

test("runSafeUpdate pins back every fingerprint package, then reports success", async () => {
  const committed = await readCommittedFingerprint();
  const calls = [];

  await runSafeUpdate({
    rootDir: "/repo",
    platform: "linux",
    execFile: recordingExec(calls, metadataMatching(committed)),
    readFile: async () => committed,
  });

  // The general refresh runs first, then the precise pins, then the recompute.
  assert.deepEqual(calls[0], ["pnpm", ["update"]]);
  assert.deepEqual(calls[1], ["cargo", ["update"]]);

  const pinned = calls
    .filter(
      ([command, args]) => command === "cargo" && args[1] === "-p" && args.includes("--precise"),
    )
    .map(([, args]) => [args[2], args[4]]);
  const expected = JSON.parse(committed).packages.map((pkg) => [pkg.name, pkg.version]);
  // One `cargo update -p <name> --precise <version>` for every recorded package,
  // at the recorded version -- not just the ~12 direct crates.
  assert.deepEqual(pinned, expected);

  const last = calls.at(-1);
  assert.equal(last[0], "cargo");
  assert.equal(last[1][0], "metadata");
  assert.ok(last[1].includes("--locked"), "the recompute must read the locked graph");
});

test("runSafeUpdate fails when the restored graph no longer matches the fingerprint", async () => {
  const committed = await readCommittedFingerprint();
  // A graph that recomputes to a different fingerprint (a lone, wrong-version
  // rolldown) stands in for a caret-drifted sibling the restore could not undo.
  const drifted = {
    packages: [{ id: "id:rolldown", name: "rolldown", version: "0.0.0", source: "registry+x" }],
    resolve: { nodes: [{ id: "id:rolldown", dependencies: [] }] },
  };

  await assert.rejects(
    runSafeUpdate({
      rootDir: "/repo",
      platform: "linux",
      execFile: recordingExec([], drifted),
      readFile: async () => committed,
    }),
    /could not restore the recorded compiler stack/u,
  );
});
