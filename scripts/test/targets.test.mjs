import assert from "node:assert/strict";
import path from "node:path";
import test from "node:test";
import {
  artifactPathForTarget,
  cargoBuildArgsForTarget,
  cargoXwinArgsForTarget,
  cargoXwinEnvForTarget,
  cargoZigbuildArgsForTarget,
  crossCompilerForTarget,
  targetInfo,
  vsixNameForTarget,
} from "../targets.mjs";

// The target table itself is data; asserting it equals a copy of itself would
// only fail when someone edits it. What is tested here is the logic layered on
// top: lookup failure, path composition, and the per-target argument branching.

test("targetInfo rejects an unsupported target", () => {
  assert.throws(() => targetInfo("freebsd-x64"), /Unsupported VSIX target: freebsd-x64/u);
});

test("artifactPathForTarget points only at the target-specific Cargo artifact", () => {
  assert.equal(
    artifactPathForTarget("C:\\repo", "linux-arm64"),
    path.join("C:\\repo", "target", "aarch64-unknown-linux-gnu", "release", "import-lens-daemon"),
  );
});

for (const [name, buildArgs, subcommand] of [
  ["cargoBuildArgsForTarget", cargoBuildArgsForTarget, "build"],
  ["cargoZigbuildArgsForTarget", cargoZigbuildArgsForTarget, "zigbuild"],
]) {
  test(`${name} passes an explicit Rust target triple`, () => {
    assert.deepEqual(buildArgs("darwin-x64"), [
      subcommand,
      "-p",
      "import-lens-daemon",
      "--release",
      "--target",
      "x86_64-apple-darwin",
    ]);
  });
}

test("cargoXwinArgsForTarget selects the clang cross-compiler only for Windows ARM64", () => {
  assert.deepEqual(cargoXwinArgsForTarget("win32-x64"), [
    "xwin",
    "build",
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    "x86_64-pc-windows-msvc",
  ]);
  assert.deepEqual(cargoXwinArgsForTarget("win32-arm64"), [
    "xwin",
    "build",
    "--cross-compiler",
    "clang",
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    "aarch64-pc-windows-msvc",
  ]);
});

test("cargoXwinEnvForTarget disables zstd intrinsics only for Windows ARM64", () => {
  assert.deepEqual(cargoXwinEnvForTarget("win32-x64", { CFLAGS: "-DEXISTING=1" }), {
    CFLAGS: "-DEXISTING=1",
  });
  assert.deepEqual(cargoXwinEnvForTarget("win32-arm64", {}), {
    CFLAGS: "-DZSTD_NO_INTRINSICS",
  });
  assert.deepEqual(cargoXwinEnvForTarget("win32-arm64", { CFLAGS: "-DEXISTING=1" }), {
    CFLAGS: "-DEXISTING=1 -DZSTD_NO_INTRINSICS",
  });
});

test("vsixNameForTarget includes package name, platform target, and version", () => {
  assert.equal(
    vsixNameForTarget({ name: "import-lens", version: "0.1.0" }, "win32-x64"),
    "dist/vsix/import-lens-win32-x64-0.1.0.vsix",
  );
});

test("crossCompilerForTarget routes the MSVC targets to xwin and the rest to zigbuild", () => {
  // zig cannot emit the MSVC ABI. This is a property of the target, not of the
  // caller, so the Docker entrypoint reads it here rather than hardcoding loops.
  assert.equal(crossCompilerForTarget("win32-x64"), "xwin");
  assert.equal(crossCompilerForTarget("win32-arm64"), "xwin");
  assert.equal(crossCompilerForTarget("linux-x64"), "zigbuild");
  assert.equal(crossCompilerForTarget("darwin-arm64"), "zigbuild");
  assert.throws(() => crossCompilerForTarget("freebsd-x64"), /Unsupported VSIX target/u);
});
