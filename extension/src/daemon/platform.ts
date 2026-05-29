export type PlatformTarget =
  | "win32-x64"
  | "win32-arm64"
  | "linux-x64"
  | "linux-arm64"
  | "darwin-x64"
  | "darwin-arm64";

export const platformTargetFrom = (platform: NodeJS.Platform, arch: NodeJS.Architecture): PlatformTarget | null => {
  if (platform === "win32" && arch === "x64") {
    return "win32-x64";
  }

  if (platform === "win32" && arch === "arm64") {
    return "win32-arm64";
  }

  if (platform === "linux" && arch === "x64") {
    return "linux-x64";
  }

  if (platform === "linux" && arch === "arm64") {
    return "linux-arm64";
  }

  if (platform === "darwin" && arch === "x64") {
    return "darwin-x64";
  }

  if (platform === "darwin" && arch === "arm64") {
    return "darwin-arm64";
  }

  return null;
};

export const currentPlatformTarget = (): PlatformTarget | null => platformTargetFrom(process.platform, process.arch);

export const daemonBinaryName = (target: PlatformTarget): string => target.startsWith("win32") ? "import-lens-daemon.exe" : "import-lens-daemon";

