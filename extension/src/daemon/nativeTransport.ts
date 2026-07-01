import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import { IpcClient } from "../ipc/client.js";
import type {
  AnalyzeDocumentRequest,
  AnalyzeDocumentResponse,
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  AnalyzeSpecifiersRequest,
  AnalyzeSpecifiersResponse,
  BatchRequest,
  BatchResponse,
  CacheCleanupRequest,
  CacheCleanupResponse,
  CacheListRequest,
  CacheListResponse,
  CacheRemoveRequest,
  CacheRemoveResponse,
  CacheStatusRequest,
  CacheStatusResponse,
  CompleteImportMembersRequest,
  CompleteImportMembersResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeDocumentRequest,
  FileSizeDocumentResponse,
  FileSizeRequest,
  FileSizeResponse,
  HelloMessage,
} from "../ipc/protocol.js";
import { protocolVersion } from "../ipc/protocol.js";
import type { Logger } from "../logging/types.js";
import { currentPlatformTarget, daemonBinaryName } from "./platform.js";
import { knownDaemonHashes } from "./knownHashes.generated.js";
import { cleanupFailedDaemonStartup, pipeDaemonProcessLogs, terminateProcess } from "./processLifecycle.js";
import { RecycleGuard } from "./recycleGuard.js";
import { recentCrashTimes, restartDelayMs, shouldEnterCrashDegradedMode } from "./restartPolicy.js";
import { resolveDaemonStartRoot } from "./startRoot.js";
import { resolveDaemonStoragePaths } from "./storagePaths.js";
import type { AnalysisTransport, DaemonState, DaemonStateEvent } from "./transport.js";

const STABLE_SESSION_RESET_MS = 60_000;
const CLEAN_RECYCLE_SESSION_MS = 30 * 60 * 1000;

export class NativeDaemonTransport implements AnalysisTransport {
  readonly #context: vscode.ExtensionContext;
  readonly #logger: Logger;
  readonly #recycleGuard: RecycleGuard;
  readonly #stateListeners = new Set<(state: DaemonState) => void>();
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
  #lastAnalysisRoot: string | undefined;

  constructor(context: vscode.ExtensionContext, logger: Logger) {
    this.#context = context;
    this.#logger = logger;
    this.#recycleGuard = new RecycleGuard(resolveDaemonStoragePaths(context).lifecycleStoragePath);
  }

  get state(): DaemonState {
    return this.#state;
  }

  readonly onDidChangeState: DaemonStateEvent = (listener) => {
    this.#stateListeners.add(listener);

    return {
      dispose: () => {
        this.#stateListeners.delete(listener);
      },
    };
  };

  async start(analysisRoot?: string): Promise<DaemonState> {
    if (this.#isDisposed) return "unavailable";
    if (this.#state === "ready" && this.#process && this.#client) return "ready";
    this.#clearRestartTimer();
    this.#clearDisconnectTimer();

    if (this.#process || this.#client) {
      this.#cleanup();
    }

    const workspaceRoot = resolveDaemonStartRoot(
      analysisRoot,
      vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
      this.#lastAnalysisRoot,
    );

    if (!workspaceRoot) {
      this.#logger.warn("No workspace or analysis root is available; daemon unavailable.");
      this.#setState("unavailable");
      return this.#state;
    }
    this.#logger.info(`Starting ImportLens daemon for workspace ${workspaceRoot}.`);

    if (await this.#recycleGuard.shouldEnterDegradedMode()) {
      this.#logger.warn("Daemon recycle loop detected. ImportLens is entering unavailable mode.");
      this.#setState("unavailable");
      return this.#state;
    }

    const target = currentPlatformTarget();

    if (!target) {
      this.#logger.warn(`Unsupported platform ${process.platform}-${process.arch}; daemon unavailable.`);
      this.#setState("unavailable");
      return this.#state;
    }

    const relativeBinaryPath = `bin/${target}/${daemonBinaryName(target)}`;
    const binaryPath = path.join(this.#context.extensionPath, relativeBinaryPath);

    if (!(await this.#verifyBinary(relativeBinaryPath, binaryPath))) {
      this.#setState("unavailable");
      return this.#state;
    }
    this.#logger.info(`Daemon binary verified: ${relativeBinaryPath}.`);

    const storagePaths = resolveDaemonStoragePaths(this.#context);
    await mkdir(storagePaths.lifecycleStoragePath, { recursive: true });
    await mkdir(storagePaths.cacheBasePath, { recursive: true });

    const pipeName = process.platform === "win32"
      ? `\\\\.\\pipe\\import-lens-${process.pid}-${randomUUID()}`
      : path.join(tmpdir(), `import-lens-${process.pid}-${randomUUID()}.sock`);

    const childProcess = spawn(binaryPath, [
      "--pipe",
      pipeName,
      "--workspace",
      workspaceRoot,
      "--storage",
      storagePaths.lifecycleStoragePath,
    ]);
    this.#process = childProcess;
    this.#logger.info(`Spawned ImportLens daemon process ${childProcess.pid ?? "unknown"}.`);
    pipeDaemonProcessLogs(childProcess, this.#logger);

    childProcess.once("exit", (code, signal) => {
      if (childProcess !== this.#process) {
        this.#logger.debug("Ignoring stale daemon process exit event.");
        return;
      }

      void this.#handleProcessExit(code, signal);
    });

    let client: IpcClient;

    try {
      client = await IpcClient.connect(pipeName, { logger: this.#logger.child({ component: "ipc" }) });
      if (childProcess !== this.#process) {
        client.dispose();
        return this.#state;
      }

      this.#client = client;
      this.#logger.info("Connected to ImportLens daemon IPC.");
    } catch (error) {
      this.#logger.warn(`Failed to connect to daemon: ${error instanceof Error ? error.message : String(error)}`);
      cleanupFailedDaemonStartup(null, childProcess);
      if (childProcess === this.#process) {
        this.#client = null;
        this.#process = null;
        this.#handleCrash();
      }
      return this.#state;
    }

    client.on("disconnect", () => {
      if (client !== this.#client) {
        this.#logger.debug("Ignoring stale daemon IPC disconnect event.");
        return;
      }

      this.#handleUnexpectedDisconnect();
    });

    try {
      this.#client.send(this.#hello(workspaceRoot));
      this.#logger.info(`Sent daemon hello using protocol v${protocolVersion}.`);
    } catch (error) {
      this.#logger.warn(`Failed to send daemon hello: ${error instanceof Error ? error.message : String(error)}`);
      cleanupFailedDaemonStartup(client, childProcess);
      if (childProcess === this.#process && client === this.#client) {
        this.#client = null;
        this.#process = null;
        this.#handleCrash();
      }
      return this.#state;
    }

    this.#lastAnalysisRoot = workspaceRoot;
    this.#setState("ready");
    this.#armStabilityReset();
    this.#armCleanRecycleReset();
    this.#logger.info("ImportLens daemon is ready.");

    return this.#state;
  }

  async #handleProcessExit(code: number | null, signal: NodeJS.Signals | null): Promise<void> {
    if (this.#isDisposed || this.#restartTimer) return;
    this.#clearDisconnectTimer();

    const gracefulExit = code === 0 && signal === null;
    const level = gracefulExit ? "info" : "warn";
    this.#logger[level](`Daemon exited with code ${code ?? "null"} signal ${signal ?? "null"}.`);
    this.#cleanup(false);

    if (gracefulExit) {
      await this.#recordCleanRecycle();
      this.#scheduleRestart(1000, "Daemon recycled; restarting in 1000ms.", "info");
      return;
    }

    this.#handleCrash();
  }

  async #recordCleanRecycle(): Promise<void> {
    try {
      await this.#recycleGuard.recordRecycle();
    } catch (error) {
      this.#logger.warn(`Failed to record daemon recycle: ${error instanceof Error ? error.message : String(error)}`);
    }
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
      this.#setState("unavailable");
      return;
    }

    this.#restartAttempt++;
    const delay = restartDelayMs(this.#restartAttempt);
    this.#scheduleRestart(delay, `Restarting daemon in ${delay}ms (attempt ${this.#restartAttempt})...`);
  }

  #scheduleRestart(delay: number, message: string, level: "info" | "warn" = "warn"): void {
    this.#setState("unavailable");
    this.#logger[level](message);
    this.#restartTimer = setTimeout(() => {
      this.#restartTimer = null;
      if (!this.#isDisposed) void this.start(this.#lastAnalysisRoot);
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

    this.#setState("unavailable");
  }

  #clearDisconnectTimer(): void {
    if (!this.#disconnectTimer) return;

    clearTimeout(this.#disconnectTimer);
    this.#disconnectTimer = null;
  }

  #clearRestartTimer(): void {
    if (!this.#restartTimer) return;

    clearTimeout(this.#restartTimer);
    this.#restartTimer = null;
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

  async sendBatch(request: BatchRequest, onPartial?: (response: BatchResponse) => void): Promise<BatchResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Batch request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Sending batch request ${request.request_id} with ${request.imports.length} import(s).`);
    return this.#client.requestBatch(request, 10000, onPartial);
  }

  async analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Document analysis ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting document analysis ${request.request_id}.`);
    return this.#client.requestAnalyzeDocument(request);
  }

  async analyzePackageJson(
    request: AnalyzePackageJsonRequest,
    onPartial?: (response: AnalyzePackageJsonResponse) => void,
  ): Promise<AnalyzePackageJsonResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`package.json analysis ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting package.json analysis ${request.request_id}.`);
    return this.#client.requestAnalyzePackageJson(request, 30000, onPartial);
  }

  async analyzeSpecifiers(request: AnalyzeSpecifiersRequest): Promise<AnalyzeSpecifiersResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Specifier analysis ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting specifier analysis ${request.request_id} for ${request.specifiers.length} import(s).`);
    return this.#client.requestAnalyzeSpecifiers(request);
  }

  async enumerateExports(request: EnumerateExportsRequest): Promise<EnumerateExportsResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Export enumeration ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting export enumeration ${request.request_id} for ${request.specifier}.`);
    return this.#client.requestExports(request);
  }

  async requestFileSize(request: FileSizeRequest): Promise<FileSizeResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Current-file size request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting current-file size ${request.request_id} for ${request.imports.length} import(s).`);
    return this.#client.requestFileSize(request);
  }

  async requestFileSizeDocument(request: FileSizeDocumentRequest): Promise<FileSizeDocumentResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Current-file size request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting current-file size ${request.request_id} from document source.`);
    return this.#client.requestFileSizeDocument(request);
  }

  async completeImportMembers(request: CompleteImportMembersRequest): Promise<CompleteImportMembersResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Import member completion ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting import member completion ${request.request_id}.`);
    return this.#client.requestCompleteImportMembers(request);
  }

  async cacheStatus(request: CacheStatusRequest): Promise<CacheStatusResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Cache status request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting cache status ${request.request_id}.`);
    return this.#client.requestCacheStatus(request);
  }

  async cleanupCache(request: CacheCleanupRequest): Promise<CacheCleanupResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Cache cleanup request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.info(`Requesting cache cleanup ${request.request_id}.`);
    return this.#client.requestCacheCleanup(request);
  }

  async listCache(request: CacheListRequest): Promise<CacheListResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Cache list request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.debug(`Requesting cache list ${request.request_id}.`);
    return this.#client.requestCacheList(request);
  }

  async removeCache(request: CacheRemoveRequest): Promise<CacheRemoveResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(`Cache remove request ${request.request_id} skipped because daemon is ${this.#state}.`);
      return null;
    }

    this.#logger.info(`Requesting cache removal ${request.request_id} with scope ${request.scope}.`);
    return this.#client.requestCacheRemove(request);
  }

  invalidatePackage(packageName: string): void {
    this.#logger.info(`Invalidating ImportLens cache for ${packageName}.`);
    this.#client?.send({ type: "cache_invalidate", package: packageName });
  }

  invalidateAll(): void {
    this.#logger.info("Invalidating entire ImportLens cache.");
    this.#client?.send({ type: "cache_invalidate_all" });
  }

  nodeModulesChanged(packageJsonPaths: readonly string[]): void {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.debug(`Skipping node_modules invalidation because daemon is ${this.#state}.`);
      return;
    }

    this.#logger.info(`Sending ${packageJsonPaths.length} node_modules package.json invalidation(s).`);
    this.#client.send({
      type: "node_modules_changed",
      package_json_paths: [...packageJsonPaths],
    });
  }

  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.debug(`Skipping package.json prewarm because daemon is ${this.#state}: ${packageJsonPath}.`);
      return;
    }

    this.#logger.debug(`Sending package.json prewarm for ${packageJsonPath}.`);
    this.#client.send({
      type: "prewarm_package_json",
      package_json_path: packageJsonPath,
      active_document_path: activeDocumentPath,
    });
  }

  async shutdown(): Promise<void> {
    this.#isDisposed = true;
    this.#clearRestartTimer();
    if (this.#stabilityTimer) clearTimeout(this.#stabilityTimer);
    if (this.#cleanRecycleTimer) clearTimeout(this.#cleanRecycleTimer);
    this.#clearDisconnectTimer();

    const client = this.#client;
    const childProcess = this.#process;

    this.#client = null;
    this.#process = null;
    this.#setState("unavailable");

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

  dispose(): void {
    void this.shutdown();
  }

  #setState(state: DaemonState): void {
    if (this.#state === state) {
      return;
    }

    this.#state = state;

    for (const listener of this.#stateListeners) {
      listener(state);
    }
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
    const storagePaths = resolveDaemonStoragePaths(this.#context);

    return {
      type: "hello",
      version: protocolVersion,
      workspace_root: workspaceRoot,
      storage_path: storagePaths.cacheBasePath,
      enable_disk_cache: config.enableDiskCache,
      cache_max_size_mb: config.cacheMaxSizeMB,
      cache_max_age_days: config.cacheMaxAgeDays,
      log_level: config.logLevel,
    };
  }
}
