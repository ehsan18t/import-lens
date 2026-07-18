import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { FINGERPRINT_PATH } from "../compiler-stack-fingerprint.mjs";
import {
  deriveRestorePins,
  lockedVersionsByName,
  outstandingPins,
  runSafeUpdate,
} from "../deps-update-safe.mjs";

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

const lockTextFrom = (packages) =>
  [
    "version = 4",
    ...packages.flatMap(({ name, version }) => [
      "",
      "[[package]]",
      `name = "${name}"`,
      `version = "${version}"`,
      'source = "registry+https://github.com/rust-lang/crates.io-index"',
    ]),
    "",
  ].join("\n");

// An in-memory stand-in for `cargo update -p <name>@<version> --precise <target>`
// reproducing the two behaviours real cargo 1.96 showed during the restore: a
// version-qualified spec moves that one copy, and a copy REFUSES to move while a
// crate named in `blockedBy` is still drifted ("failed to select a version for
// the requirement ..."). The refusal is what makes a single pass unable to
// converge, so it is the behaviour the sweep exists to survive.
const fakeCargoLock = (initialPackages, { blockedBy = {} } = {}) => {
  const packages = initialPackages.map((pkg) => ({ ...pkg }));

  return {
    text: () => lockTextFrom(packages),
    pin: (crate, locked, target) => {
      const copy = packages.find((pkg) => pkg.name === crate && pkg.version === locked);
      if (!copy) {
        const error = new Error("cargo failed");
        error.stderr = `error: package ID specification \`${crate}@${locked}\` did not match any packages`;
        throw error;
      }
      for (const [blocker, blockedVersion] of blockedBy[crate] ?? []) {
        if (packages.some((pkg) => pkg.name === blocker && pkg.version === blockedVersion)) {
          const error = new Error("cargo failed");
          error.stderr = `error: failed to select a version for the requirement \`${crate} = "^${locked}"\`\nrequired by package \`${blocker} v${blockedVersion}\``;
          throw error;
        }
      }
      copy.version = target;
    },
  };
};

// Records every exec as [command, args]; answers `cargo metadata` with the
// supplied graph, applies `cargo update -p` against the fake lock when one is
// supplied, and answers everything else with empty stdout. Handles both call
// shapes: (cmd, options) and (cmd, args, options).
const recordingExec =
  (calls, metadata, lock = undefined) =>
  async (command, argsOrOptions) => {
    const args = Array.isArray(argsOrOptions) ? argsOrOptions : [];
    calls.push([command, args]);
    if (command === "cargo" && args[0] === "metadata") {
      return { stdout: JSON.stringify(metadata) };
    }
    if (lock && command === "cargo" && args[0] === "update" && args[1] === "-p") {
      const [crate, locked] = args[2].split("@");
      lock.pin(crate, locked, args[4]);
    }
    return { stdout: "" };
  };

const readingFrom = (fingerprintText, lock) => async (filePath) =>
  String(filePath).endsWith("Cargo.lock") ? lock.text() : fingerprintText;

const pinSpecs = (calls) =>
  calls
    .filter(
      ([command, args]) => command === "cargo" && args[1] === "-p" && args.includes("--precise"),
    )
    .map(([, args]) => [args[2], args[4]]);

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

test("lockedVersionsByName records every locked copy, including a duplicated crate", () => {
  const locked = lockedVersionsByName(
    lockTextFrom([
      { name: "oxc", version: "0.139.0" },
      { name: "oxc", version: "0.140.0" },
      { name: "rolldown", version: "1.1.5" },
    ]),
  );

  assert.deepEqual(locked.get("oxc"), ["0.139.0", "0.140.0"]);
  assert.deepEqual(locked.get("rolldown"), ["1.1.5"]);
  assert.equal(locked.get("absent"), undefined);
});

test("outstandingPins skips crates already at the recorded version and names every drifted copy", () => {
  const lockText = lockTextFrom([
    { name: "oxc", version: "0.139.0" },
    { name: "oxc", version: "0.140.0" },
    { name: "rolldown_sourcemap", version: "1.2.0" },
    { name: "rolldown", version: "1.1.5" },
  ]);

  const outstanding = outstandingPins(lockText, [
    ["oxc", "0.139.0"],
    ["rolldown_sourcemap", "1.1.5"],
    // Already at the recorded version: nothing to do, so no pin at all.
    ["rolldown", "1.1.5"],
  ]);

  assert.deepEqual(outstanding, [
    ["oxc", "0.140.0", "0.139.0"],
    ["rolldown_sourcemap", "1.2.0", "1.1.5"],
  ]);
});

test("runSafeUpdate pins back every drifted fingerprint package, then reports success", async () => {
  const committed = await readCommittedFingerprint();
  const recorded = JSON.parse(committed).packages;
  const calls = [];
  // Every recorded crate drifted to one fictional version, so the restore has to
  // emit a pin for each -- the coverage the previous unconditional loop gave.
  const lock = fakeCargoLock(recorded.map((pkg) => ({ name: pkg.name, version: "9.9.9" })));

  await runSafeUpdate({
    rootDir: "/repo",
    platform: "linux",
    execFile: recordingExec(calls, metadataMatching(committed), lock),
    readFile: readingFrom(committed, lock),
  });

  // The general refresh runs first, then the precise pins, then the recompute.
  assert.deepEqual(calls[0], ["pnpm", ["update"]]);
  assert.deepEqual(calls[1], ["cargo", ["update"]]);

  assert.deepEqual(
    pinSpecs(calls),
    recorded.map((pkg) => [`${pkg.name}@9.9.9`, pkg.version]),
  );

  const last = calls.at(-1);
  assert.equal(last[0], "cargo");
  assert.equal(last[1][0], "metadata");
  assert.ok(last[1].includes("--locked"), "the recompute must read the locked graph");
});

// GUARD: a bare `-p <name>` spec is what broke the restore in the field --
// `cargo update -p oxc --precise 0.139.0` aborts with "specification `oxc` is
// ambiguous" the moment a drifted dependent has pulled a second copy in, which
// is the ONLY situation this command runs in. Every spec must carry its version.
test("runSafeUpdate never emits a version-less package spec", async () => {
  const committed = await readCommittedFingerprint();
  const calls = [];
  const lock = fakeCargoLock(
    JSON.parse(committed).packages.map((pkg) => ({ name: pkg.name, version: "9.9.9" })),
  );

  await runSafeUpdate({
    rootDir: "/repo",
    platform: "linux",
    execFile: recordingExec(calls, metadataMatching(committed), lock),
    readFile: readingFrom(committed, lock),
  });

  const bare = pinSpecs(calls).filter(([spec]) => !spec.includes("@"));
  assert.deepEqual(bare, [], "an ambiguous bare spec aborts the whole restore");
});

// The field failure, end to end: `cargo update` drifted rolldown's caret-ranged
// sibling to 1.2.0, that sibling requires `oxc ^0.140.0`, and so a second `oxc`
// landed beside our pinned 0.139.0. The fingerprint's name order reaches `oxc`
// first, where it cannot move until the sibling is restored -- so a single pass
// can never converge, no matter how the spec is written. Sweeping is what fixes
// it. Collapse the loop back to one pass and this goes red.
test("runSafeUpdate converges when a drifted dependent blocks a pin on the first sweep", async () => {
  const committed = JSON.stringify(
    {
      packages: [
        { name: "oxc", version: "0.139.0", source: "registry+https://example/index" },
        { name: "rolldown", version: "1.1.5", source: "registry+https://example/index" },
        { name: "rolldown_sourcemap", version: "1.1.5", source: "registry+https://example/index" },
      ],
    },
    null,
    2,
  ).concat("\n");

  const calls = [];
  const lock = fakeCargoLock(
    [
      { name: "oxc", version: "0.139.0" },
      { name: "oxc", version: "0.140.0" },
      { name: "rolldown", version: "1.1.5" },
      { name: "rolldown_sourcemap", version: "1.2.0" },
    ],
    // `oxc` cannot leave 0.140.0 while rolldown_sourcemap is still at 1.2.0.
    { blockedBy: { oxc: [["rolldown_sourcemap", "1.2.0"]] } },
  );

  await runSafeUpdate({
    rootDir: "/repo",
    platform: "linux",
    execFile: recordingExec(calls, metadataMatching(committed), lock),
    readFile: readingFrom(committed, lock),
  });

  const specs = pinSpecs(calls);
  assert.deepEqual(
    specs[0],
    ["oxc@0.140.0", "0.139.0"],
    "the blocked pin is still attempted first -- fingerprint order is alphabetical",
  );
  assert.ok(
    specs.some(([spec]) => spec === "rolldown_sourcemap@1.2.0"),
    "the dependent that dragged the second copy in must itself be restored",
  );
  // The discriminator against a single pass: `oxc` is attempted once while
  // blocked and again after the sibling is restored. Drop the sweep and the
  // second attempt never happens -- and cargo leaves the second copy in place.
  assert.equal(
    specs.filter(([spec]) => spec === "oxc@0.140.0").length,
    2,
    "the blocked pin must be retried once its blocker is restored",
  );
});

test("runSafeUpdate fails when the restored graph no longer matches the fingerprint", async () => {
  const committed = await readCommittedFingerprint();
  // A graph that recomputes to a different fingerprint (a lone, wrong-version
  // rolldown) stands in for a caret-drifted sibling the restore could not undo.
  const drifted = {
    packages: [{ id: "id:rolldown", name: "rolldown", version: "0.0.0", source: "registry+x" }],
    resolve: { nodes: [{ id: "id:rolldown", dependencies: [] }] },
  };
  const lock = fakeCargoLock(
    JSON.parse(committed).packages.map((pkg) => ({ name: pkg.name, version: pkg.version })),
  );

  await assert.rejects(
    runSafeUpdate({
      rootDir: "/repo",
      platform: "linux",
      execFile: recordingExec([], drifted, lock),
      readFile: readingFrom(committed, lock),
    }),
    /could not restore the recorded compiler stack/u,
  );
});

// A restore that gives up must say WHICH pin cargo refused and why; without it
// the operator sees only "the graph does not match" and has nothing to act on.
test("runSafeUpdate reports the refusal cargo gave for a pin that could not land", async () => {
  const committed = JSON.stringify(
    {
      packages: [
        { name: "oxc", version: "0.139.0", source: "registry+https://example/index" },
        { name: "rolldown", version: "1.1.5", source: "registry+https://example/index" },
      ],
    },
    null,
    2,
  ).concat("\n");

  const drifted = {
    packages: [{ id: "id:rolldown", name: "rolldown", version: "0.0.0", source: "registry+x" }],
    resolve: { nodes: [{ id: "id:rolldown", dependencies: [] }] },
  };
  const lock = fakeCargoLock(
    [
      { name: "oxc", version: "0.140.0" },
      { name: "rolldown", version: "1.1.5" },
    ],
    // Nothing ever restores this blocker, so the pin can never land.
    { blockedBy: { oxc: [["rolldown", "1.1.5"]] } },
  );

  await assert.rejects(
    runSafeUpdate({
      rootDir: "/repo",
      platform: "linux",
      execFile: recordingExec([], drifted, lock),
      readFile: readingFrom(committed, lock),
    }),
    /Unrestored pins:[\s\S]*oxc 0\.140\.0 -> 0\.139\.0.*failed to select a version/u,
  );
});
