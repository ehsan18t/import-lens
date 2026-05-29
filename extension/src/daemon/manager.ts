import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";
import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import type { ImportLensLogger } from "../logger.js";
import { IpcClient } from "../ipc/client.js";
import type { BatchRequest, BatchResponse, HelloMessage } from "../ipc/protocol.js";
import { protocolVersion } from "../ipc/protocol.js";
import { daemonBinaryName, currentPlatformTarget } from "./platform.js";
import { knownDaemonHashes } from "./knownHashes.generated.js";
import { RecycleGuard } from "./recycleGuard.js";
import { recentCrashTimes, restartDelayMs, shouldEnterCrashDegradedMode } from "./restartPolicy.js";

export type DaemonState = "ready" | "unavailable";

const STABLE_SESSION_RESET_MS = 60_000;
const CLEAN_RECYCLE_SESSION_MS = 30 * 60 * 1000;

export class DaemonManager implements vscode.Disposable {
  readonly #context: vscode.ExtensionContext;
  readonly #logger: ImportLensLogger;
  readonly #recycleGuard: RecycleGuard;
  #process: ChildProcessWithoutNullStreams | null = null;
  #client: IpcClient | null = null;
  #state: DaemonState = "unavailable";
  #isDisposed = false;
  #restartAttempt = 0;
  #crashTimes: number[] = [];
  #restartTimer: NodeJS.Timeout | null = null;
  #stabilityTimer: NodeJS.Timeout | null = null;
  #cleanRecycleTimer: NodeJS.Timeout | null = null;
  #disconnectTimer: NodeJS.Timeout | null = null;

  constructor(context: vscode.ExtensionContext, logger: ImportLensLogger) {
    this.#context = context;
    this.#logger = logger;
    this.#recycleGuard = new RecycleGuard(context.globalStorageUri.fsPath);
  }

  get state(): DaemonState {
    return this.#state;
  }

  async start(): Promise<DaemonState> {
    if (this.#isDisposed) return "unavailable";
    if (this.#state === "ready" && this.#process && this.#client) return "ready";

    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

    if (!workspaceRoot) {
      this.#logger.warn("No workspace folder is open; daemon unavailable.");
      this.#state = "unavailable";
      return this.#state;
    }

    if (await this.#recycleGuard.shouldEnterDegradedMode()) {
      this.#logger.warn("Daemon recycle loop detected. ImportLens is entering unavailable mode.");
      this.#state = "unavailable";
      return this.#state;
    }

    const target = currentPlatformTarget();

    if (!target) {
      this.#logger.warn(`Unsupported platform ${process.platform}-${process.arch}; daemon unavailable.`);
      this.#state = "unavailable";
      return this.#state;
    }

    const relativeBinaryPath = `bin/${target}/${daemonBinaryName(target)}`;
    const binaryPath = path.join(this.#context.extensionPath, relativeBinaryPath);

    if (!(await this.#verifyBinary(relativeBinaryPath, binaryPath))) {
      this.#state = "unavailable";
      return this.#state;
    }

    await mkdir(this.#context.globalStorageUri.fsPath, { recursive: true });

    const pipeName = process.platform === "win32"
      ? `\\\\.\\pipe\\import-lens-${process.pid}-${randomUUID()}`
      : path.join(this.#context.globalStorageUri.fsPath, `import-lens-${process.pid}-${randomUUID()}.sock`);

    this.#process = spawn(binaryPath, [
      "--pipe",
      pipeName,
      "--workspace",
      workspaceRoot,
      "--storage",
      this.#context.globalStorageUri.fsPath,
    ]);

    this.#process.once("exit", (code, signal) => {
      this.#handleProcessExit(code, signal);
    });

    try {
      this.#client = await IpcClient.connect(pipeName);
    } catch (error) {
      this.#logger.warn(`Failed to connect to daemon: ${error instanceof Error ? error.message : String(error)}`);
      this.#handleCrash();
      return this.#state;
    }

    this.#client.on("disconnect", (error) => {
      this.#logger.warn(`IPC disconnected: ${error instanceof Error ? error.message : String(error)}`);
      this.#handleUnexpectedDisconnect();
    });
    this.#client.send(this.#hello(workspaceRoot));
    this.#state = "ready";
    this.#armStabilityReset();
    this.#armCleanRecycleReset();

    return this.#state;
  }

  #handleProcessExit(code: number | null, signal: NodeJS.Signals | null): void {
    if (this.#isDisposed || this.#restartTimer) return;
    this.#clearDisconnectTimer();

    const gracefulExit = code === 0 && signal === null;
    const level = gracefulExit ? "info" : "warn";
    this.#logger[level](`Daemon exited with code ${code ?? "null"} signal ${signal ?? "null"}.`);
    this.#cleanup(false);

    if (gracefulExit) {
      this.#scheduleRestart(1000, "Daemon recycled; restarting in 1000ms.", "info");
      return;
    }

    this.#handleCrash();
  }

  #handleUnexpectedDisconnect(): void {
    if (this.#isDisposed || this.#restartTimer || this.#disconnectTimer) return;

    this.#disconnectTimer = setTimeout(() => {
      this.#disconnectTimer = null;

      if (this.#isDisposed || this.#restartTimer) return;

      this.#cleanup();
      this.#handleCrash();
    }, 100);
  }

  #handleCrash(): void {
    const now = Date.now();
    this.#crashTimes = recentCrashTimes([...this.#crashTimes, now], now);

    if (shouldEnterCrashDegradedMode(this.#crashTimes, now)) {
      this.#logger.error("Daemon crashed three times within 60 seconds. ImportLens is entering unavailable mode.");
      this.#state = "unavailable";
      return;
    }

    this.#restartAttempt++;
    const delay = restartDelayMs(this.#restartAttempt);
    this.#scheduleRestart(delay, `Restarting daemon in ${delay}ms (attempt ${this.#restartAttempt})...`);
  }

  #scheduleRestart(delay: number, message: string, level: "info" | "warn" = "warn"): void {
    this.#state = "unavailable";
    this.#logger[level](message);
    this.#restartTimer = setTimeout(() => {
      this.#restartTimer = null;
      if (!this.#isDisposed) void this.start();
    }, delay);
  }

  #cleanup(killProcess = true): void {
    this.#clearDisconnectTimer();
    const client = this.#client;
    const childProcess = this.#process;

    this.#client = null;
    this.#process = null;
    client?.dispose();

    if (killProcess) {
      childProcess?.kill();
    }

    this.#state = "unavailable";
  }

  #clearDisconnectTimer(): void {
    if (!this.#disconnectTimer) return;

    clearTimeout(this.#disconnectTimer);
    this.#disconnectTimer = null;
  }

  #armStabilityReset(): void {
    if (this.#stabilityTimer) clearTimeout(this.#stabilityTimer);
    this.#stabilityTimer = setTimeout(() => {
      this.#restartAttempt = 0;
      this.#crashTimes = [];
    }, STABLE_SESSION_RESET_MS);
  }

  #armCleanRecycleReset(): void {
    if (this.#cleanRecycleTimer) clearTimeout(this.#cleanRecycleTimer);
    this.#cleanRecycleTimer = setTimeout(() => {
      void this.#recycleGuard.resetAfterCleanSession();
    }, CLEAN_RECYCLE_SESSION_MS);
  }

  async sendBatch(request: BatchRequest): Promise<BatchResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      return null;
    }

    return this.#client.requestBatch(request);
  }

  invalidatePackage(packageName: string): void {
    this.#client?.send({ type: "cache_invalidate", package: packageName });
  }

  invalidateAll(): void {
    this.#client?.send({ type: "cache_invalidate_all" });
  }

  async dispose(): Promise<void> {
    this.#isDisposed = true;
    if (this.#restartTimer) clearTimeout(this.#restartTimer);
    if (this.#stabilityTimer) clearTimeout(this.#stabilityTimer);
    if (this.#cleanRecycleTimer) clearTimeout(this.#cleanRecycleTimer);
    this.#clearDisconnectTimer();

    const client = this.#client;
    const childProcess = this.#process;

    this.#client = null;
    this.#process = null;
    this.#state = "unavailable";

    try {
      client?.send({ type: "shutdown" });
    } catch (error) {
      this.#logger.warn(`Failed to send daemon shutdown: ${error instanceof Error ? error.message : String(error)}`);
    }

    if (childProcess) {
      await terminateProcess(childProcess);
    }

    client?.dispose();
  }

  async #verifyBinary(relativePath: string, binaryPath: string): Promise<boolean> {
    const expectedHash = knownDaemonHashes[relativePath];

    if (!expectedHash) {
      this.#logger.warn(`No trusted hash is available for ${relativePath}. Build the daemon and run pnpm hash:daemon.`);
      return false;
    }

    try {
      const actualHash = createHash("sha256").update(await readFile(binaryPath)).digest("hex");

      if (actualHash !== expectedHash) {
        this.#logger.error(`Daemon hash mismatch for ${relativePath}.`);
        return false;
      }

      return true;
    } catch (error) {
      this.#logger.warn(`Daemon binary is unavailable at ${binaryPath}: ${error instanceof Error ? error.message : String(error)}`);
      return false;
    }
  }

  #hello(workspaceRoot: string): HelloMessage {
    const config = getImportLensConfig();

    return {
      type: "hello",
      version: protocolVersion,
      workspace_root: workspaceRoot,
      storage_path: this.#context.globalStorageUri.fsPath,
      enable_disk_cache: config.enableDiskCache,
      log_level: config.logLevel,
    };
  }
}

const waitForExit = (
  childProcess: ChildProcessWithoutNullStreams,
  timeoutMs: number,
): Promise<boolean> => {
  if (childProcess.exitCode !== null || childProcess.signalCode !== null) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    const onExit = (): void => {
      clearTimeout(timer);
      resolve(true);
    };
    const timer = setTimeout(() => {
      childProcess.off("exit", onExit);
      resolve(false);
    }, timeoutMs);

    childProcess.once("exit", onExit);
  });
};

const terminateProcess = async (childProcess: ChildProcessWithoutNullStreams): Promise<void> => {
  if (await waitForExit(childProcess, 5000)) {
    return;
  }

  if (process.platform === "win32") {
    childProcess.kill();
    await waitForExit(childProcess, 2000);
    return;
  }

  childProcess.kill("SIGTERM");

  if (!(await waitForExit(childProcess, 2000))) {
    childProcess.kill("SIGKILL");
    await waitForExit(childProcess, 1000);
  }
};
