import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import {
  knownHashesSource,
  parseKnownHashesSource,
  updateKnownDaemonHashes,
} from "../daemon-hashes.mjs";

const tempRepo = () => mkdtempSync(path.join(os.tmpdir(), "import-lens-hashes-"));

const writeBinary = (repoRoot, relativePath, contents) => {
  const absolutePath = path.join(repoRoot, relativePath);
  mkdirSync(path.dirname(absolutePath), { recursive: true });
  writeFileSync(absolutePath, contents);
};

const sha256 = (contents) => createHash("sha256").update(contents).digest("hex");

test("updateKnownDaemonHashes preserves unrelated target hashes during a target refresh", () => {
  const repoRoot = tempRepo();
  const winBinary = "win-daemon";

  try {
    writeBinary(repoRoot, "bin/win32-x64/import-lens-daemon.exe", winBinary);
    const existingSource = knownHashesSource({
      "bin/darwin-arm64/import-lens-daemon": "old-darwin-hash",
    });
    const { hashes, source } = updateKnownDaemonHashes({
      repoRoot,
      selectedTargets: ["win32-x64"],
      existingSource,
    });

    assert.equal(hashes["bin/darwin-arm64/import-lens-daemon"], "old-darwin-hash");
    assert.equal(hashes["bin/win32-x64/import-lens-daemon.exe"], sha256(winBinary));
    assert.deepEqual(parseKnownHashesSource(source), hashes);
  } finally {
    rmSync(repoRoot, { force: true, recursive: true });
  }
});

test("updateKnownDaemonHashes throws instead of stripping when a selected target's binary is missing", () => {
  const repoRoot = tempRepo();

  try {
    // win32-x64 is built locally; win32-arm64 is not.
    writeBinary(repoRoot, "bin/win32-x64/import-lens-daemon.exe", "win-x64");
    const existingSource = knownHashesSource({
      "bin/win32-arm64/import-lens-daemon.exe": "old-arm64-hash",
    });

    assert.throws(
      () =>
        updateKnownDaemonHashes({
          repoRoot,
          selectedTargets: ["win32-x64", "win32-arm64"],
          existingSource,
        }),
      /daemon binary missing for win32-arm64/u,
    );

    // Aborting before any write leaves the previously trusted hash intact.
    assert.equal(
      parseKnownHashesSource(existingSource)["bin/win32-arm64/import-lens-daemon.exe"],
      "old-arm64-hash",
    );
  } finally {
    rmSync(repoRoot, { force: true, recursive: true });
  }
});

test("knownHashesSource emits deterministic sorted TypeScript", () => {
  assert.equal(
    knownHashesSource({
      "bin/win32-x64/import-lens-daemon.exe": "win",
      "bin/darwin-arm64/import-lens-daemon": "darwin",
    }),
    [
      "export const knownDaemonHashes: Readonly<Record<string, string>> = {",
      "  \"bin/darwin-arm64/import-lens-daemon\": \"darwin\",",
      "  \"bin/win32-x64/import-lens-daemon.exe\": \"win\"",
      "};",
      "",
    ].join("\n"),
  );
});
