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

  constructor(context: vscode.ExtensionContext, logger: ImportLensLogger) {
    this.#context = context;
    this.#logger = logger;
  }

  get state(): DaemonState {
    return this.#state;
  }

  async start(): Promise<DaemonState> {
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
      this.#client?.dispose();
      this.#client = null;
      this.#process = null;
      this.#state = "unavailable";
    });

    this.#client = await IpcClient.connect(pipeName);
    this.#client.on("disconnect", (error) => {
      this.#logger.warn(`IPC disconnected: ${error instanceof Error ? error.message : String(error)}`);
      this.#state = "unavailable";
    });
    this.#client.send(this.#hello(workspaceRoot));
    this.#state = "ready";
    return this.#state;
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
    this.#client?.send({ type: "shutdown" });
    this.#client?.dispose();
    this.#process?.kill();
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

