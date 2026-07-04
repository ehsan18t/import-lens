import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import {
  artifactPathForTarget,
  cargoBuildArgsForTarget,
  cargoZigbuildArgsForTarget,
  platformTargets,
  targetInfo,
  vsixNameForTarget,
} from "../targets.mjs";

test("platformTargets lists every supported native VSIX target", () => {
  assert.deepEqual(platformTargets, [
    "win32-x64",
    "win32-arm64",
    "linux-x64",
    "linux-arm64",
    "darwin-x64",
    "darwin-arm64",
  ]);
});

test("targetInfo maps VSIX targets to Rust targets and binaries", () => {
  assert.deepEqual(targetInfo("win32-arm64"), {
    platformTarget: "win32-arm64",
    rustTarget: "aarch64-pc-windows-msvc",
    binaryName: "import-lens-daemon.exe",
  });
  assert.deepEqual(targetInfo("linux-arm64"), {
    platformTarget: "linux-arm64",
    rustTarget: "aarch64-unknown-linux-gnu",
    binaryName: "import-lens-daemon",
  });
});

test("artifactPathForTarget points only at the target-specific Cargo artifact", () => {
  assert.equal(
    artifactPathForTarget("C:\\repo", "linux-arm64"),
    path.join("C:\\repo", "target", "aarch64-unknown-linux-gnu", "release", "import-lens-daemon"),
  );
});

test("cargoBuildArgsForTarget uses explicit Rust target triples", () => {
  assert.deepEqual(cargoBuildArgsForTarget("darwin-x64"), [
    "build",
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    "x86_64-apple-darwin",
  ]);
});

test("cargoZigbuildArgsForTarget uses explicit Rust target triples", () => {
  assert.deepEqual(cargoZigbuildArgsForTarget("linux-arm64"), [
    "zigbuild",
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    "aarch64-unknown-linux-gnu",
  ]);
});

test("vsixNameForTarget includes package name, platform target, and version", () => {
  assert.equal(
    vsixNameForTarget({ name: "import-lens", version: "0.1.0" }, "win32-x64"),
    "dist/vsix/import-lens-win32-x64-0.1.0.vsix",
  );
});
