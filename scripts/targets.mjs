import path from "node:path";

export const platformTargets = [
  "win32-x64",
  "win32-arm64",
  "linux-x64",
  "linux-arm64",
  "darwin-x64",
  "darwin-arm64",
];

const targets = new Map([
  [
    "win32-x64",
    {
      platformTarget: "win32-x64",
      rustTarget: "x86_64-pc-windows-msvc",
      binaryName: "import-lens-daemon.exe",
    },
  ],
  [
    "win32-arm64",
    {
      platformTarget: "win32-arm64",
      rustTarget: "aarch64-pc-windows-msvc",
      binaryName: "import-lens-daemon.exe",
    },
  ],
  [
    "linux-x64",
    {
      platformTarget: "linux-x64",
      rustTarget: "x86_64-unknown-linux-gnu",
      binaryName: "import-lens-daemon",
    },
  ],
  [
    "linux-arm64",
    {
      platformTarget: "linux-arm64",
      rustTarget: "aarch64-unknown-linux-gnu",
      binaryName: "import-lens-daemon",
    },
  ],
  [
    "darwin-x64",
    {
      platformTarget: "darwin-x64",
      rustTarget: "x86_64-apple-darwin",
      binaryName: "import-lens-daemon",
    },
  ],
  [
    "darwin-arm64",
    {
      platformTarget: "darwin-arm64",
      rustTarget: "aarch64-apple-darwin",
      binaryName: "import-lens-daemon",
    },
  ],
]);

const runtimeTargets = new Map([
  ["win32:x64", "win32-x64"],
  ["win32:arm64", "win32-arm64"],
  ["linux:x64", "linux-x64"],
  ["linux:arm64", "linux-arm64"],
  ["darwin:x64", "darwin-x64"],
  ["darwin:arm64", "darwin-arm64"],
]);

export const currentPlatformTarget = () =>
  runtimeTargets.get(`${process.platform}:${process.arch}`) ?? null;

export const targetInfo = (platformTarget) => {
  const info = targets.get(platformTarget);

  if (!info) {
    throw new Error(`Unsupported VSIX target: ${platformTarget}`);
  }

  return info;
};

export const artifactPathForTarget = (repoRoot, platformTarget) => {
  const info = targetInfo(platformTarget);
  return path.join(repoRoot, "target", info.rustTarget, "release", info.binaryName);
};

export const cargoBuildArgsForTarget = (platformTarget) => {
  const info = targetInfo(platformTarget);

  return ["build", "-p", "import-lens-daemon", "--release", "--target", info.rustTarget];
};

export const cargoZigbuildArgsForTarget = (platformTarget) => {
  const info = targetInfo(platformTarget);

  return ["zigbuild", "-p", "import-lens-daemon", "--release", "--target", info.rustTarget];
};

const appendCFlag = (existingValue, flag) => (existingValue ? `${existingValue} ${flag}` : flag);

// zig cannot target the MSVC ABI, so the Windows targets cross-compile from
// Linux with cargo-xwin against a splatted Windows SDK/CRT. The artifact lands
// at the same target/<triple>/release path as a native build, so
// artifactPathForTarget needs no special-casing.
export const cargoXwinArgsForTarget = (platformTarget) => {
  const info = targetInfo(platformTarget);
  // ring's build script trips over the clang-cl CFLAGS shape for Windows ARM64
  // in the Linux builder, while cargo-xwin's clang backend compiles it cleanly.
  const crossCompilerArgs = platformTarget === "win32-arm64" ? ["--cross-compiler", "clang"] : [];

  return [
    "xwin",
    "build",
    ...crossCompilerArgs,
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    info.rustTarget,
  ];
};

export const cargoXwinEnvForTarget = (platformTarget, baseEnv = process.env) => {
  targetInfo(platformTarget);

  if (platformTarget !== "win32-arm64") {
    return baseEnv;
  }

  return {
    ...baseEnv,
    // zstd's ARM64 NEON intrinsics produce unresolved SIMDe helper symbols under
    // the clang cargo-xwin backend; scalar code keeps this cross-build linkable.
    CFLAGS: appendCFlag(baseEnv.CFLAGS, "-DZSTD_NO_INTRINSICS"),
  };
};

// zig cannot emit the MSVC ABI, so Windows cross-compiles with cargo-xwin while
// the unix targets go through cargo-zigbuild. Which compiler a target needs is a
// property of the target; callers read it here instead of hardcoding the split.
export const crossCompilerForTarget = (platformTarget) => {
  targetInfo(platformTarget);
  return platformTarget.startsWith("win32") ? "xwin" : "zigbuild";
};

// All build artifacts live under dist/ (target/ is the one Rust-convention
// exception). These relative segments are the single source for every script.
export const vsixDir = "dist/vsix";
export const stagingDir = "dist/staging";
// Shipped artifacts. These repo-relative paths are also the in-VSIX layout: in
// dev the extension resolves against the repo root, so the two cannot diverge.
// The extension mirrors daemonRoot in extension/src/daemon/platform.ts and the
// CLI in cli/importlens.mjs (neither can import this module at runtime); the
// daemon-path-contract test keeps all three in lockstep.
export const daemonRoot = "dist/bin";
export const extensionBundle = "dist/extension/extension.cjs";

export const relativeDaemonPath = (platformTarget) => {
  const { binaryName } = targetInfo(platformTarget);
  return `${daemonRoot}/${platformTarget}/${binaryName}`;
};

export const vsixNameForTarget = (manifest, platformTarget) =>
  `${vsixDir}/${manifest.name}-${platformTarget}-${manifest.version}.vsix`;
