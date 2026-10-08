import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { IpcClient } from "../ipc/client.js";
import type { Logger } from "../logging/types.js";
import { knownDaemonHashes } from "./knownHashes.generated.js";
import type { DaemonChildProcess } from "./processLifecycle.js";

/**
 * The operating-system edges of a daemon start: hashing the binary, spawning it, and opening its
 * IPC endpoint. The transport owns every decision around them (single flight, crash breaker,
 * degraded mode); tests replace only these edges.
 */
export interface DaemonLauncher {
  readonly verifyBinary: (
    relativePath: string,
    binaryPath: string,
    logger: Logger,
  ) => Promise<boolean>;
  readonly spawn: (binaryPath: string, args: readonly string[]) => DaemonChildProcess;
  readonly connect: (pipeName: string, logger: Logger) => Promise<IpcClient>;
}

const verifyDaemonBinary = async (
  relativePath: string,
  binaryPath: string,
  logger: Logger,
): Promise<boolean> => {
  const expectedHash = knownDaemonHashes[relativePath];

  if (!expectedHash) {
    logger.warn(
      `No trusted hash is available for ${relativePath}. Build the daemon and run pnpm hash:daemon.`,
    );
    return false;
  }

  try {
    const actualHash = createHash("sha256")
      .update(await readFile(binaryPath))
      .digest("hex");

    if (actualHash !== expectedHash) {
      logger.error(`Daemon hash mismatch for ${relativePath}.`);
      return false;
    }

    return true;
  } catch (error) {
    logger.warn(
      `Daemon binary is unavailable at ${binaryPath}: ${error instanceof Error ? error.message : String(error)}`,
    );
    return false;
  }
};

export const nativeDaemonLauncher: DaemonLauncher = {
  verifyBinary: verifyDaemonBinary,
  spawn: (binaryPath, args) => spawn(binaryPath, [...args]),
  connect: (pipeName, logger) => IpcClient.connect(pipeName, { logger }),
};
