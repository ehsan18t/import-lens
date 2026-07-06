import { EventEmitter } from "node:events";
import net from "node:net";
import type { Logger } from "../logging/types.js";
import { encodeFrame, FrameDecoder } from "./codec.js";
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
  ClientMessage,
  CompleteImportMembersRequest,
  CompleteImportMembersResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeDocumentRequest,
  FileSizeDocumentResponse,
  FileSizeResponse,
  RefreshedResultsResponse,
  RefreshRegistryHintsRequest,
  RefreshRegistryHintsResponse,
  WorkspaceReportRequest,
  WorkspaceReportResponse,
} from "./protocol.js";

export interface IpcClientConnectOptions {
  timeoutMs?: number;
  logger?: Pick<Logger, "debug" | "warn">;
}

interface PendingPackageJsonRequest {
  resolve: (response: AnalyzePackageJsonResponse) => void;
  reject: (error: Error) => void;
  onPartial?: (response: AnalyzePackageJsonResponse) => void;
  resetTimeout: () => void;
}

interface PendingRequest<TResponse> {
  resolve: (response: TResponse) => void;
  reject: (error: Error) => void;
}

interface PendingRegistryHintRefreshRequest {
  resolve: (response: RefreshRegistryHintsResponse) => void;
  reject: (error: Error) => void;
  onPartial?: (response: RefreshRegistryHintsResponse) => void;
  resetTimeout: () => void;
}

export class IpcClient extends EventEmitter {
  readonly #socket: net.Socket;
  readonly #decoder = new FrameDecoder();
  readonly #documentPending = new Map<number, PendingRequest<AnalyzeDocumentResponse>>();
  readonly #packageJsonPending = new Map<number, PendingPackageJsonRequest>();
  readonly #specifiersPending = new Map<number, PendingRequest<AnalyzeSpecifiersResponse>>();
  readonly #exportsPending = new Map<number, PendingRequest<EnumerateExportsResponse>>();
  readonly #fileSizeDocumentPending = new Map<number, PendingRequest<FileSizeDocumentResponse>>();
  readonly #completionPending = new Map<number, PendingRequest<CompleteImportMembersResponse>>();
  readonly #cacheStatusPending = new Map<number, PendingRequest<CacheStatusResponse>>();
  readonly #cacheListPending = new Map<number, PendingRequest<CacheListResponse>>();
  readonly #cacheRemovePending = new Map<number, PendingRequest<CacheRemoveResponse>>();
  readonly #registryHintRefreshPending = new Map<number, PendingRegistryHintRefreshRequest>();
  readonly #workspaceReportPending = new Map<number, PendingRequest<WorkspaceReportResponse>>();
  readonly #logger?: Pick<Logger, "debug" | "warn">;
  #closed = false;
  #disposed = false;

  private constructor(socket: net.Socket, logger?: Pick<Logger, "debug" | "warn">) {
    super();
    this.#socket = socket;
    this.#logger = logger;
    this.#socket.on("data", (chunk: Buffer) => this.#handleData(chunk));
    this.#socket.on("close", () => this.#handleClose(new Error("IPC socket closed")));
    this.#socket.on("error", (error) => this.#handleClose(error));
  }

  static connect(pipeName: string, options: IpcClientConnectOptions = {}): Promise<IpcClient> {
    const timeoutMs = options.timeoutMs ?? 2000;
    const logger = options.logger;
    const startedAt = Date.now();

    return new Promise((resolve, reject) => {
      const attempt = (): void => {
        const socket = net.createConnection(pipeName);
        let settled = false;

        socket.once("connect", () => {
          settled = true;
          logger?.debug(`IPC connected to ${pipeName}.`);
          resolve(new IpcClient(socket, logger));
        });
        socket.once("error", (error) => {
          socket.destroy();

          if (settled) {
            return;
          }

          if (Date.now() - startedAt >= timeoutMs) {
            logger?.warn(`IPC connect timed out after ${timeoutMs}ms: ${error.message}`);
            reject(error);
            return;
          }

          setTimeout(attempt, 50);
        });
      };

      attempt();
    });
  }

  send(message: ClientMessage): void {
    this.#socket.write(encodeFrame(message));
  }

  requestAnalyzeDocument(
    request: AnalyzeDocumentRequest,
    timeoutMs = 10000,
  ): Promise<AnalyzeDocumentResponse> {
    return this.#requestWithPending(this.#documentPending, request, timeoutMs);
  }

  requestAnalyzePackageJson(
    request: AnalyzePackageJsonRequest,
    timeoutMs = 10000,
    onPartial?: (response: AnalyzePackageJsonResponse) => void,
  ): Promise<AnalyzePackageJsonResponse> {
    return new Promise((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;

      const resetTimeout = (): void => {
        if (timer) {
          clearTimeout(timer);
        }
        timer = setTimeout(() => {
          if (this.#packageJsonPending.has(request.request_id)) {
            this.#packageJsonPending.delete(request.request_id);
            reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
          }
        }, timeoutMs);
      };

      resetTimeout();
      this.#packageJsonPending.set(request.request_id, {
        resolve: (response) => {
          clearTimeout(timer);
          resolve(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
        onPartial,
        resetTimeout,
      });
      this.send(request);
    });
  }

  requestRefreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    timeoutMs = 30000,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse> {
    return new Promise((resolve, reject) => {
      let timer: NodeJS.Timeout | undefined;

      const resetTimeout = (): void => {
        if (timer) {
          clearTimeout(timer);
        }
        timer = setTimeout(() => {
          if (this.#registryHintRefreshPending.has(request.request_id)) {
            this.#registryHintRefreshPending.delete(request.request_id);
            reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
          }
        }, timeoutMs);
      };

      resetTimeout();
      this.#registryHintRefreshPending.set(request.request_id, {
        resolve: (response) => {
          if (timer) {
            clearTimeout(timer);
          }
          resolve(response);
        },
        reject: (error) => {
          if (timer) {
            clearTimeout(timer);
          }
          reject(error);
        },
        onPartial,
        resetTimeout,
      });
      this.send(request);
    });
  }

  requestAnalyzeSpecifiers(
    request: AnalyzeSpecifiersRequest,
    timeoutMs = 10000,
  ): Promise<AnalyzeSpecifiersResponse> {
    return this.#requestWithPending(this.#specifiersPending, request, timeoutMs);
  }

  requestExports(
    request: EnumerateExportsRequest,
    timeoutMs = 10000,
  ): Promise<EnumerateExportsResponse> {
    return this.#requestWithPending(this.#exportsPending, request, timeoutMs);
  }

  requestFileSizeDocument(
    request: FileSizeDocumentRequest,
    timeoutMs = 10000,
  ): Promise<FileSizeDocumentResponse> {
    return this.#requestWithPending(this.#fileSizeDocumentPending, request, timeoutMs);
  }

  requestCompleteImportMembers(
    request: CompleteImportMembersRequest,
    timeoutMs = 10000,
  ): Promise<CompleteImportMembersResponse> {
    return this.#requestWithPending(this.#completionPending, request, timeoutMs);
  }

  requestCacheStatus(request: CacheStatusRequest, timeoutMs = 10000): Promise<CacheStatusResponse> {
    return this.#requestWithPending(this.#cacheStatusPending, request, timeoutMs);
  }

  requestCacheList(request: CacheListRequest, timeoutMs = 10000): Promise<CacheListResponse> {
    return this.#requestWithPending(this.#cacheListPending, request, timeoutMs);
  }

  requestCacheRemove(request: CacheRemoveRequest, timeoutMs = 30000): Promise<CacheRemoveResponse> {
    return this.#requestWithPending(this.#cacheRemovePending, request, timeoutMs);
  }

  requestWorkspaceReport(
    request: WorkspaceReportRequest,
    timeoutMs = 60000,
  ): Promise<WorkspaceReportResponse> {
    return this.#requestWithPending(this.#workspaceReportPending, request, timeoutMs);
  }

  #requestWithPending<TRequest extends { request_id: number }, TResponse>(
    pendingMap: Map<number, PendingRequest<TResponse>>,
    request: TRequest & ClientMessage,
    timeoutMs: number,
  ): Promise<TResponse> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (pendingMap.has(request.request_id)) {
          pendingMap.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);

      pendingMap.set(request.request_id, {
        resolve: (response) => {
          clearTimeout(timer);
          resolve(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.send(request);
    });
  }

  dispose(): void {
    this.#disposed = true;
    this.#socket.destroy();
    this.#handleClose(new Error("IPC client disposed"), false);
  }

  #handleData(chunk: Buffer): void {
    let messages: unknown[];

    try {
      messages = this.#decoder.push(chunk);
    } catch (error) {
      const normalized = error instanceof Error ? error : new Error(String(error));

      if (normalized.message.includes("too large")) {
        this.#logger?.warn(normalized.message);
      }

      this.#handleClose(normalized);
      return;
    }

    for (const message of messages) {
      if (isCacheStatusResponse(message)) {
        this.#resolvePending(this.#cacheStatusPending, message);
        continue;
      }

      if (isCacheListResponse(message)) {
        this.#resolvePending(this.#cacheListPending, message);
        continue;
      }

      if (isCacheRemoveResponse(message)) {
        this.#resolvePending(this.#cacheRemovePending, message);
        continue;
      }

      if (isAnalyzePackageJsonResponse(message)) {
        const pending = this.#packageJsonPending.get(message.request_id);

        if (!pending) {
          continue;
        }

        if (isPackageJsonStreamingPartial(message)) {
          pending.resetTimeout();
          pending.onPartial?.(message);
          this.emit("packageJsonPartial", message);
          continue;
        }

        this.#packageJsonPending.delete(message.request_id);
        pending.resolve(message);
        continue;
      }

      if (isCompleteImportMembersResponse(message)) {
        this.#resolvePending(this.#completionPending, message);
        continue;
      }

      if (isFileSizeDocumentResponse(message)) {
        this.#resolvePending(this.#fileSizeDocumentPending, message);
        continue;
      }

      if (isRefreshRegistryHintsResponse(message)) {
        const pending = this.#registryHintRefreshPending.get(message.request_id);

        if (!pending) {
          continue;
        }

        if (isRegistryHintRefreshPartial(message)) {
          pending.resetTimeout();
          pending.onPartial?.(message);
          continue;
        }

        this.#registryHintRefreshPending.delete(message.request_id);
        pending.resolve(message);
        continue;
      }

      if (isWorkspaceReportResponse(message)) {
        this.#resolvePending(this.#workspaceReportPending, message);
        continue;
      }

      if (isRefreshedResultsResponse(message)) {
        // Unsolicited SWR push — no pending request to resolve. Emit for the
        // subscriber that owns the AnalysisStore to merge in place.
        this.emit("refreshedResults", message);
        continue;
      }

      if (isAnalyzeDocumentResponse(message)) {
        if (this.#resolvePending(this.#documentPending, message)) {
          continue;
        }

        this.#resolvePending(this.#specifiersPending, message);
        continue;
      }

      if (!isEnumerateExportsResponse(message)) {
        continue;
      }

      this.#resolvePending(this.#exportsPending, message);
    }
  }

  #resolvePending<TResponse extends { request_id: number }>(
    pendingMap: Map<number, PendingRequest<TResponse>>,
    response: TResponse,
  ): boolean {
    const pending = pendingMap.get(response.request_id);

    if (!pending) {
      return false;
    }

    pendingMap.delete(response.request_id);
    pending.resolve(response);
    return true;
  }

  #handleClose(error: Error, emitDisconnect = true): void {
    if (this.#closed) {
      return;
    }

    this.#closed = true;
    this.#decoder.reset();

    for (const pending of this.#documentPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#packageJsonPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#specifiersPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#exportsPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#fileSizeDocumentPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#completionPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#cacheStatusPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#cacheListPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#cacheRemovePending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#registryHintRefreshPending.values()) {
      pending.reject(error);
    }
    this.#registryHintRefreshPending.clear();

    for (const pending of this.#workspaceReportPending.values()) {
      pending.reject(error);
    }
    this.#workspaceReportPending.clear();

    this.#documentPending.clear();
    this.#packageJsonPending.clear();
    this.#specifiersPending.clear();
    this.#exportsPending.clear();
    this.#fileSizeDocumentPending.clear();
    this.#completionPending.clear();
    this.#cacheStatusPending.clear();
    this.#cacheListPending.clear();
    this.#cacheRemovePending.clear();

    if (emitDisconnect && !this.#disposed) {
      this.#logger?.warn(`IPC disconnected: ${error.message}`);
      this.emit("disconnect", error);
    }
  }
}

const isPackageJsonStreamingPartial = (response: AnalyzePackageJsonResponse): boolean =>
  Array.isArray(response.indexes) && response.indexes.length > 0;

const isRefreshRegistryHintsResponse = (value: unknown): value is RefreshRegistryHintsResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<RefreshRegistryHintsResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.results) &&
    candidate.results.every(
      (result) =>
        !!result &&
        typeof result === "object" &&
        !!(result as { target?: unknown }).target &&
        typeof (result as { target: { name?: unknown } }).target.name === "string",
    ) &&
    (candidate.indexes === undefined ||
      (Array.isArray(candidate.indexes) &&
        candidate.indexes.every((index) => typeof index === "number"))) &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isRegistryHintRefreshPartial = (response: RefreshRegistryHintsResponse): boolean =>
  Array.isArray(response.indexes) && response.indexes.length > 0;

const isRefreshedResultsResponse = (value: unknown): value is RefreshedResultsResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<RefreshedResultsResponse>;
  return (
    candidate.type === "refreshed_results" &&
    typeof candidate.workspace_root === "string" &&
    typeof candidate.document_path === "string" &&
    Array.isArray(candidate.results)
  );
};

const isWorkspaceReportResponse = (value: unknown): value is WorkspaceReportResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<WorkspaceReportResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.rows) &&
    !!candidate.summary &&
    typeof candidate.summary === "object" &&
    Array.isArray(candidate.summary.treemap) &&
    Array.isArray(candidate.summary.duplicateImports) &&
    Array.isArray(candidate.summary.sharedModules) &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isAnalyzeDocumentResponse = (value: unknown): value is AnalyzeDocumentResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<AnalyzeDocumentResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.imports) &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics) &&
    candidate.imports.every(
      (item) =>
        !!item &&
        typeof item === "object" &&
        "detected" in item &&
        typeof (item as { status?: unknown }).status === "string",
    )
  );
};

const isAnalyzePackageJsonResponse = (value: unknown): value is AnalyzePackageJsonResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<AnalyzePackageJsonResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.sections) &&
    Array.isArray(candidate.states) &&
    (candidate.indexes === undefined ||
      candidate.indexes.every((index) => typeof index === "number")) &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isEnumerateExportsResponse = (value: unknown): value is EnumerateExportsResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<EnumerateExportsResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    typeof candidate.specifier === "string" &&
    Array.isArray(candidate.exports) &&
    candidate.exports.every((exportedName) => typeof exportedName === "string") &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isFileSizeDocumentResponse = (value: unknown): value is FileSizeDocumentResponse => {
  if (!isFileSizeResponse(value)) {
    return false;
  }

  return Array.isArray((value as Partial<FileSizeDocumentResponse>).states);
};

const isFileSizeResponse = (value: unknown): value is FileSizeResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<FileSizeResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    typeof candidate.raw_bytes === "number" &&
    typeof candidate.minified_bytes === "number" &&
    typeof candidate.gzip_bytes === "number" &&
    typeof candidate.brotli_bytes === "number" &&
    typeof candidate.zstd_bytes === "number" &&
    Array.isArray(candidate.imports) &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isCompleteImportMembersResponse = (
  value: unknown,
): value is CompleteImportMembersResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<CompleteImportMembersResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    (candidate.specifier === null || typeof candidate.specifier === "string") &&
    Array.isArray(candidate.exports) &&
    candidate.exports.every((exportedName) => typeof exportedName === "string") &&
    Array.isArray(candidate.imported_names) &&
    candidate.imported_names.every((importedName) => typeof importedName === "string") &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isCacheShardInfo = (value: unknown): boolean => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<NonNullable<CacheStatusResponse["current_project"]>>;
  return (
    typeof candidate.shard_id === "string" &&
    typeof candidate.project_root === "string" &&
    typeof candidate.normalized_root === "string" &&
    typeof candidate.cache_path === "string" &&
    typeof candidate.size_bytes === "number" &&
    (candidate.last_used_millis === null || typeof candidate.last_used_millis === "number") &&
    typeof candidate.loaded === "boolean"
  );
};

const isCacheOperationResult = (value: unknown): boolean => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<CacheRemoveResponse["removed"][number]>;
  return (
    typeof candidate.shard_id === "string" &&
    typeof candidate.project_root === "string" &&
    typeof candidate.cache_path === "string" &&
    typeof candidate.removed === "boolean" &&
    (candidate.error === null || typeof candidate.error === "string")
  );
};

const hasCacheResponseBase = (
  value: unknown,
): value is {
  version: number;
  request_id: number;
  error: string | null;
  diagnostics: unknown[];
} => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as {
    version?: unknown;
    request_id?: unknown;
    error?: unknown;
    diagnostics?: unknown;
  };
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    (candidate.error === null || typeof candidate.error === "string") &&
    Array.isArray(candidate.diagnostics)
  );
};

const isCacheStatusResponse = (value: unknown): value is CacheStatusResponse => {
  if (!hasCacheResponseBase(value)) {
    return false;
  }

  const candidate = value as Partial<CacheStatusResponse>;
  return (
    typeof candidate.total_size_bytes === "number" &&
    typeof candidate.project_count === "number" &&
    typeof candidate.max_size_mb === "number" &&
    (candidate.current_project === null || isCacheShardInfo(candidate.current_project))
  );
};

const isCacheListResponse = (value: unknown): value is CacheListResponse => {
  if (!hasCacheResponseBase(value)) {
    return false;
  }

  const candidate = value as Partial<CacheListResponse>;
  return Array.isArray(candidate.shards) && candidate.shards.every(isCacheShardInfo);
};

const isCacheRemoveResponse = (value: unknown): value is CacheRemoveResponse => {
  if (!hasCacheResponseBase(value)) {
    return false;
  }

  const candidate = value as Partial<CacheRemoveResponse>;
  return (
    Array.isArray(candidate.removed) &&
    candidate.removed.every(isCacheOperationResult) &&
    Array.isArray(candidate.failed) &&
    candidate.failed.every(isCacheOperationResult)
  );
};
