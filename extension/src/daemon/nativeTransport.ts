import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import type { ImportLensConfig } from "../config.js";
import { IpcClient } from "../ipc/client.js";
import type {
  AnalyzeDocumentRequest,
  AnalyzeDocumentResponse,
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  AnalyzeSpecifiersRequest,
  AnalyzeSpecifiersResponse,
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
  HelloMessage,
  RefreshedResultsResponse,
  RefreshRegistryHintsRequest,
  RefreshRegistryHintsResponse,
  WorkspaceReportRequest,
  WorkspaceReportResponse,
} from "../ipc/protocol.js";
import { protocolVersion } from "../ipc/protocol.js";
import type { Logger } from "../logging/types.js";
import { knownDaemonHashes } from "./knownHashes.generated.js";
import { currentPlatformTarget, daemonRelativePath } from "./platform.js";
import {
  cleanupFailedDaemonStartup,
  pipeDaemonProcessLogs,
  terminateProcess,
} from "./processLifecycle.js";
import { RecycleGuard } from "./recycleGuard.js";
import { recentCrashTimes, restartDelayMs, shouldEnterCrashDegradedMode } from "./restartPolicy.js";
import { resolveDaemonStartRoot } from "./startRoot.js";
import { type DaemonStorageContext, resolveDaemonStoragePaths } from "./storagePaths.js";
import type {
  AnalysisTransport,
  DaemonRefreshedResultsEvent,
  DaemonState,
  DaemonStateEvent,
} from "./transport.js";

const STABLE_SESSION_RESET_MS = 60_000;
const CLEAN_RECYCLE_SESSION_MS = 30 * 60 * 1000;
const PACKAGE_JSON_ANALYSIS_TIMEOUT_MS = 300_000;
// A whole-workspace report scans, resolves, graphs, minifies, and compresses every file, so it
// can exceed the 60s per-document budget on large monorepos.
// The subset of `vscode.ExtensionContext` this transport reads. Typing against
// it (rather than the full `ExtensionContext`) keeps the module free of any
// `vscode` value/type dependency, so it loads under the extension-host-free
// test runner; a real `ExtensionContext` still satisfies it structurally.
type DaemonHostContext = DaemonStorageContext & { readonly extensionPath: string };

const WORKSPACE_REPORT_TIMEOUT_MS = 300_000;

export class NativeDaemonTransport implements AnalysisTransport {
  readonly #context: DaemonHostContext;
  readonly #logger: Logger;
  readonly #recycleGuard: RecycleGuard;
  readonly #stateListeners = new Set<(state: DaemonState) => void>();
  readonly #refreshedResultsListeners = new Set<(message: RefreshedResultsResponse) => void>();
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
  // Injected so this module depends on `vscode` for types only and can be
  // constructed under the extension-host-free test runner. In production the
  // manager supplies the active workspace folder's path.
  readonly #workspaceFallbackRoot: () => string | undefined;
  readonly #getConfig: () => ImportLensConfig;

  constructor(
    context: DaemonHostContext,
    logger: Logger,
    workspaceFallbackRoot: () => string | undefined,
    getConfig: () => ImportLensConfig,
  ) {
    this.#context = context;
    this.#logger = logger;
    this.#workspaceFallbackRoot = workspaceFallbackRoot;
    this.#getConfig = getConfig;
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

  readonly onRefreshedResults: DaemonRefreshedResultsEvent = (listener) => {
    this.#refreshedResultsListeners.add(listener);

    return {
      dispose: () => {
        this.#refreshedResultsListeners.delete(listener);
      },
    };
  };

  async start(analysisRoot?: string): Promise<DaemonState> {
    // An explicit start() (including the one DaemonManager.restart() performs
    // after shutdown()) is a request to run, so clear the disposal latch that
    // shutdown() set. The auto-restart timer still checks #isDisposed before
    // calling start(), so a genuine dispose still prevents self-resurrection.
    this.#isDisposed = false;
    if (this.#state === "ready" && this.#process && this.#client) return "ready";
    this.#clearRestartTimer();
    this.#clearDisconnectTimer();

    if (this.#process || this.#client) {
      this.#cleanup();
    }

    const workspaceRoot = resolveDaemonStartRoot(
      analysisRoot,
      this.#workspaceFallbackRoot(),
      this.#lastAnalysisRoot,
    );

    if (!workspaceRoot) {
      this.#logger.warn("No workspace or analysis root is available; daemon unavailable.");
      this.#setState("unavailable");
      return this.#state;
    }
    this.#logger.info(`Starting Import Lens daemon for workspace ${workspaceRoot}.`);

    if (await this.#recycleGuard.shouldEnterDegradedMode()) {
      this.#logger.warn("Daemon recycle loop detected. Import Lens is entering unavailable mode.");
      this.#setState("unavailable");
      return this.#state;
    }

    const target = currentPlatformTarget();

    if (!target) {
      this.#logger.warn(
        `Unsupported platform ${process.platform}-${process.arch}; daemon unavailable.`,
      );
      this.#setState("unavailable");
      return this.#state;
    }

    const relativeBinaryPath = daemonRelativePath(target);
    const binaryPath = path.join(this.#context.extensionPath, relativeBinaryPath);

    if (!(await this.#verifyBinary(relativeBinaryPath, binaryPath))) {
      this.#setState("unavailable");
      return this.#state;
    }
    this.#logger.info(`Daemon binary verified: ${relativeBinaryPath}.`);

    const storagePaths = resolveDaemonStoragePaths(this.#context);
    await mkdir(storagePaths.lifecycleStoragePath, { recursive: true });
    await mkdir(storagePaths.cacheBasePath, { recursive: true });

    const pipeName =
      process.platform === "win32"
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
    this.#logger.info(`Spawned Import Lens daemon process ${childProcess.pid ?? "unknown"}.`);
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
      client = await IpcClient.connect(pipeName, {
        logger: this.#logger.child({ component: "ipc" }),
      });
      if (childProcess !== this.#process) {
        client.dispose();
        return this.#state;
      }

      this.#client = client;
      this.#logger.info("Connected to Import Lens daemon IPC.");
    } catch (error) {
      this.#logger.warn(
        `Failed to connect to daemon: ${error instanceof Error ? error.message : String(error)}`,
      );
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

    client.on("refreshedResults", (message: RefreshedResultsResponse) => {
      if (client !== this.#client) {
        this.#logger.debug("Ignoring stale daemon refreshed-results event.");
        return;
      }

      this.#emitRefreshedResults(message);
    });

    try {
      this.#client.send(this.#hello(workspaceRoot));
      this.#logger.info(`Sent daemon hello using protocol v${protocolVersion}.`);
    } catch (error) {
      this.#logger.warn(
        `Failed to send daemon hello: ${error instanceof Error ? error.message : String(error)}`,
      );
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
    this.#logger.info("Import Lens daemon is ready.");

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
      this.#logger.warn(
        `Failed to record daemon recycle: ${error instanceof Error ? error.message : String(error)}`,
      );
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
      this.#logger.error(
        "Daemon crashed three times within 60 seconds. Import Lens is entering unavailable mode.",
      );
      this.#setState("unavailable");
      return;
    }

    this.#restartAttempt++;
    const delay = restartDelayMs(this.#restartAttempt);
    this.#scheduleRestart(
      delay,
      `Restarting daemon in ${delay}ms (attempt ${this.#restartAttempt})...`,
    );
  }

  #scheduleRestart(delay: number, message: string, level: "info" | "warn" = "warn"): void {
    this.#setState("unavailable");
    this.#logger[level](message);
    this.#restartTimer = setTimeout(() => {
      this.#restartTimer = null;
      if (this.#isDisposed) return;
      void this.start(this.#lastAnalysisRoot).catch((error: unknown) => {
        this.#logger.warn(
          `Scheduled daemon restart failed: ${error instanceof Error ? error.message : String(error)}`,
        );
        this.#setState("unavailable");
      });
    }, delay);
  }

  #cleanup(killProcess = true): void {
    this.#clearDisconnectTimer();
    // Clear the session-scoped stability/clean-recycle timers so a timer armed
    // by a previous (now-ended) session cannot later fire and reset the crash
    // breaker and backoff after a crash.
    if (this.#stabilityTimer) {
      clearTimeout(this.#stabilityTimer);
      this.#stabilityTimer = null;
    }
    if (this.#cleanRecycleTimer) {
      clearTimeout(this.#cleanRecycleTimer);
      this.#cleanRecycleTimer = null;
    }
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

  async analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Document analysis ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
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
      this.#logger.warn(
        `package.json analysis ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(`Requesting package.json analysis ${request.request_id}.`);
    return this.#client.requestAnalyzePackageJson(
      request,
      PACKAGE_JSON_ANALYSIS_TIMEOUT_MS,
      onPartial,
    );
  }

  async analyzeSpecifiers(
    request: AnalyzeSpecifiersRequest,
  ): Promise<AnalyzeSpecifiersResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Specifier analysis ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(
      `Requesting specifier analysis ${request.request_id} for ${request.specifiers.length} import(s).`,
    );
    return this.#client.requestAnalyzeSpecifiers(request);
  }

  async enumerateExports(
    request: EnumerateExportsRequest,
  ): Promise<EnumerateExportsResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Export enumeration ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(
      `Requesting export enumeration ${request.request_id} for ${request.specifier}.`,
    );
    return this.#client.requestExports(request);
  }

  async requestFileSizeDocument(
    request: FileSizeDocumentRequest,
  ): Promise<FileSizeDocumentResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Current-file size request ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(`Requesting current-file size ${request.request_id} from document source.`);
    return this.#client.requestFileSizeDocument(request);
  }

  async completeImportMembers(
    request: CompleteImportMembersRequest,
  ): Promise<CompleteImportMembersResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Import member completion ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(`Requesting import member completion ${request.request_id}.`);
    return this.#client.requestCompleteImportMembers(request);
  }

  async cacheStatus(request: CacheStatusRequest): Promise<CacheStatusResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Cache status request ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(`Requesting cache status ${request.request_id}.`);
    return this.#client.requestCacheStatus(request);
  }

  async listCache(request: CacheListRequest): Promise<CacheListResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Cache list request ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(`Requesting cache list ${request.request_id}.`);
    return this.#client.requestCacheList(request);
  }

  async removeCache(request: CacheRemoveRequest): Promise<CacheRemoveResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Cache remove request ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.info(
      `Requesting cache removal ${request.request_id} with scope ${request.scope}.`,
    );
    return this.#client.requestCacheRemove(request);
  }

  async refreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Registry hint refresh ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    const names = request.targets.map((target) => target.name);
    const preview =
      names.length <= 8
        ? names.join(", ")
        : `${names.slice(0, 8).join(", ")}, +${names.length - 8} more`;
    this.#logger.debug(
      `Requesting registry hint refresh ${request.request_id} for ${request.targets.length} package(s): ${preview}.`,
    );
    return this.#client.requestRefreshRegistryHints(request, 30000, onPartial);
  }

  async requestWorkspaceReport(
    request: WorkspaceReportRequest,
  ): Promise<WorkspaceReportResponse | null> {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.warn(
        `Workspace report ${request.request_id} skipped because daemon is ${this.#state}.`,
      );
      return null;
    }

    this.#logger.debug(
      `Requesting workspace report ${request.request_id} for ${request.workspace_root}.`,
    );
    return this.#client.requestWorkspaceReport(request, WORKSPACE_REPORT_TIMEOUT_MS);
  }

  invalidatePackage(packageName: string): void {
    this.#logger.info(`Invalidating Import Lens cache for ${packageName}.`);
    this.#client?.send({ type: "cache_invalidate", package: packageName });
  }

  invalidateAll(): void {
    this.#logger.info("Invalidating entire Import Lens cache.");
    this.#client?.send({ type: "cache_invalidate_all" });
  }

  nodeModulesChanged(
    packageJsonPaths: readonly string[],
    tsconfigPaths: readonly string[] = [],
  ): void {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.debug(`Skipping node_modules invalidation because daemon is ${this.#state}.`);
      return;
    }

    this.#logger.info(
      `Sending ${packageJsonPaths.length} node_modules package.json and ${tsconfigPaths.length} tsconfig invalidation(s).`,
    );
    this.#client.send({
      type: "node_modules_changed",
      package_json_paths: [...packageJsonPaths],
      tsconfig_paths: [...tsconfigPaths],
    });
  }

  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void {
    if (!this.#client || this.#state !== "ready") {
      this.#logger.debug(
        `Skipping package.json prewarm because daemon is ${this.#state}: ${packageJsonPath}.`,
      );
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
      this.#logger.warn(
        `Failed to send daemon shutdown: ${error instanceof Error ? error.message : String(error)}`,
      );
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

  #emitRefreshedResults(message: RefreshedResultsResponse): void {
    for (const listener of this.#refreshedResultsListeners) {
      listener(message);
    }
  }

  async #verifyBinary(relativePath: string, binaryPath: string): Promise<boolean> {
    const expectedHash = knownDaemonHashes[relativePath];

    if (!expectedHash) {
      this.#logger.warn(
        `No trusted hash is available for ${relativePath}. Build the daemon and run pnpm hash:daemon.`,
      );
      return false;
    }

    try {
      const actualHash = createHash("sha256")
        .update(await readFile(binaryPath))
        .digest("hex");

      if (actualHash !== expectedHash) {
        this.#logger.error(`Daemon hash mismatch for ${relativePath}.`);
        return false;
      }

      return true;
    } catch (error) {
      this.#logger.warn(
        `Daemon binary is unavailable at ${binaryPath}: ${error instanceof Error ? error.message : String(error)}`,
      );
      return false;
    }
  }

  #hello(workspaceRoot: string): HelloMessage {
    const config = this.#getConfig();
    const storagePaths = resolveDaemonStoragePaths(this.#context);

    return {
      type: "hello",
      version: protocolVersion,
      workspace_root: workspaceRoot,
      storage_path: storagePaths.cacheBasePath,
      enable_disk_cache: config.enableDiskCache,
      cache_max_size_mb: config.cacheMaxSizeMB,
      registry_cache_max_size_mb: config.registryCacheMaxSizeMB,
      log_level: config.logLevel,
    };
  }
}
