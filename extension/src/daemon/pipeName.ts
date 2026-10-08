import { randomBytes, randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import path from "node:path";

// The longest socket path bind() accepts: `sun_path` holds 104 bytes on macOS and the BSDs and
// 108 on Linux, and both counts include the terminating NUL.
const unixSocketPathLimit = (platform: NodeJS.Platform): number =>
  platform === "linux" ? 107 : 103;

const FALLBACK_SOCKET_DIRECTORY = "/tmp";

/**
 * The IPC endpoint for one daemon spawn. On Unix the name is kept short because `$TMPDIR` alone
 * is ~48 bytes on macOS; a path over the `sun_path` limit fails bind() and the daemon never
 * starts, so an overlong temp directory falls back to `/tmp`.
 */
export const daemonPipeName = (
  platform: NodeJS.Platform = process.platform,
  pid: number = process.pid,
  tempDirectory: string = tmpdir(),
): string => {
  if (platform === "win32") {
    return `\\\\.\\pipe\\import-lens-${pid}-${randomUUID()}`;
  }

  const fileName = `il-${randomBytes(6).toString("hex")}.sock`;
  const preferred = path.posix.join(tempDirectory, fileName);

  return Buffer.byteLength(preferred) <= unixSocketPathLimit(platform)
    ? preferred
    : path.posix.join(FALLBACK_SOCKET_DIRECTORY, fileName);
};
