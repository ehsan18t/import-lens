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

export type DaemonState = "ready" | "unavailable";

export class DaemonManager implements vscode.Disposable {
  readonly #context: vscode.ExtensionContext;
  readonly #logger: ImportLensLogger;
  #process: ChildProcessWithoutNullStreams | null = null;
  #client: IpcClient | null = null;
  #state: DaemonState = "unavailable";
  #isDisposed = false;
  #restarts = 0;
  #restartTimer: NodeJS.Timeout | null = null;
  #stabilityTimer: NodeJS.Timeout | null = null;

  constructor(context: vscode.ExtensionContext, logger: ImportLensLogger) {
    this.#context = context;
    this.#logger = logger;
  }

  get state(): DaemonState {
    return this.#state;
  }

  async start(): Promise<DaemonState> {
    if (this.#isDisposed) return "unavailable";

    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

    if (!workspaceRoot) {
      this.#logger.warn("No workspace folder is open; daemon unavailable.");
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
      this.#logger.warn(`Daemon exited with code ${code ?? "null"} signal ${signal ?? "null"}.`);
      this.#handleCrash();
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
      this.#handleCrash();
    });
    this.#client.send(this.#hello(workspaceRoot));
    this.#state = "ready";

    if (this.#stabilityTimer) clearTimeout(this.#stabilityTimer);
    this.#stabilityTimer = setTimeout(() => {
      this.#restarts = 0;
    }, 5000);

    return this.#state;
  }

  #handleCrash(): void {
    if (this.#isDisposed || this.#restartTimer) return;
    this.#cleanup();

    if (this.#restarts >= 5) {
      this.#logger.error("Daemon crashed too many times. Giving up.");
      this.#state = "unavailable";
      return;
    }

    this.#restarts++;
    const delay = Math.min(250 * (2 ** this.#restarts), 10000);
    this.#logger.warn(`Restarting daemon in ${delay}ms (attempt ${this.#restarts})...`);
    this.#restartTimer = setTimeout(() => {
      this.#restartTimer = null;
      if (!this.#isDisposed) void this.start();
    }, delay);
  }

  #cleanup(): void {
    this.#client?.dispose();
    this.#client = null;
    this.#process?.kill();
    this.#process = null;
    this.#state = "unavailable";
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
    
    this.#client?.send({ type: "shutdown" });
    this.#cleanup();
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

