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
      oxcParserBinding: "@oxc-parser/binding-win32-x64-msvc",
    },
  ],
  [
    "win32-arm64",
    {
      platformTarget: "win32-arm64",
      rustTarget: "aarch64-pc-windows-msvc",
      binaryName: "import-lens-daemon.exe",
      oxcParserBinding: "@oxc-parser/binding-win32-arm64-msvc",
    },
  ],
  [
    "linux-x64",
    {
      platformTarget: "linux-x64",
      rustTarget: "x86_64-unknown-linux-gnu",
      binaryName: "import-lens-daemon",
      oxcParserBinding: "@oxc-parser/binding-linux-x64-gnu",
    },
  ],
  [
    "linux-arm64",
    {
      platformTarget: "linux-arm64",
      rustTarget: "aarch64-unknown-linux-gnu",
      binaryName: "import-lens-daemon",
      oxcParserBinding: "@oxc-parser/binding-linux-arm64-gnu",
    },
  ],
  [
    "darwin-x64",
    {
      platformTarget: "darwin-x64",
      rustTarget: "x86_64-apple-darwin",
      binaryName: "import-lens-daemon",
      oxcParserBinding: "@oxc-parser/binding-darwin-x64",
    },
  ],
  [
    "darwin-arm64",
    {
      platformTarget: "darwin-arm64",
      rustTarget: "aarch64-apple-darwin",
      binaryName: "import-lens-daemon",
      oxcParserBinding: "@oxc-parser/binding-darwin-arm64",
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

export const currentPlatformTarget = () => runtimeTargets.get(`${process.platform}:${process.arch}`) ?? null;

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

  return [
    "build",
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    info.rustTarget,
  ];
};

export const cargoZigbuildArgsForTarget = (platformTarget) => {
  const info = targetInfo(platformTarget);

  return [
    "zigbuild",
    "-p",
    "import-lens-daemon",
    "--release",
    "--target",
    info.rustTarget,
  ];
};

export const vsixNameForTarget = (manifest, platformTarget) =>
  `builds/${manifest.name}-${platformTarget}-${manifest.version}.vsix`;
