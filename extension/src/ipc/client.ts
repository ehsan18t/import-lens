import { EventEmitter } from "node:events";
import net from "node:net";
import type {
  AnalyzeDocumentRequest,
  AnalyzeDocumentResponse,
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  AnalyzeSpecifiersRequest,
  AnalyzeSpecifiersResponse,
  BatchRequest,
  BatchResponse,
  ClientMessage,
  CompleteImportMembersRequest,
  CompleteImportMembersResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeDocumentRequest,
  FileSizeDocumentResponse,
  FileSizeRequest,
  FileSizeResponse,
} from "./protocol.js";
import type { Logger } from "../logging/types.js";
import { FrameDecoder, encodeFrame } from "./codec.js";

export interface IpcClientConnectOptions {
  timeoutMs?: number;
  logger?: Pick<Logger, "debug" | "warn">;
}

interface PendingBatchRequest {
  resolve: (response: BatchResponse) => void;
  reject: (error: Error) => void;
  onPartial?: (response: BatchResponse) => void;
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

export class IpcClient extends EventEmitter {
  readonly #socket: net.Socket;
  readonly #decoder = new FrameDecoder();
  readonly #batchPending = new Map<number, PendingBatchRequest>();
  readonly #documentPending = new Map<number, PendingRequest<AnalyzeDocumentResponse>>();
  readonly #packageJsonPending = new Map<number, PendingPackageJsonRequest>();
  readonly #specifiersPending = new Map<number, PendingRequest<AnalyzeSpecifiersResponse>>();
  readonly #exportsPending = new Map<number, PendingRequest<EnumerateExportsResponse>>();
  readonly #fileSizePending = new Map<number, PendingRequest<FileSizeResponse>>();
  readonly #fileSizeDocumentPending = new Map<number, PendingRequest<FileSizeDocumentResponse>>();
  readonly #completionPending = new Map<number, PendingRequest<CompleteImportMembersResponse>>();
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

  requestBatch(
    request: BatchRequest,
    timeoutMs = 10000,
    onPartial?: (response: BatchResponse) => void,
  ): Promise<BatchResponse> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.#batchPending.has(request.request_id)) {
          this.#batchPending.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);

      this.#batchPending.set(request.request_id, {
        resolve: (response) => {
          clearTimeout(timer);
          resolve(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
        onPartial,
      });
      this.send(request);
    });
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

      timer = setTimeout(() => {
        if (this.#packageJsonPending.has(request.request_id)) {
          this.#packageJsonPending.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);

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

  requestFileSize(
    request: FileSizeRequest,
    timeoutMs = 10000,
  ): Promise<FileSizeResponse> {
    return this.#requestWithPending(this.#fileSizePending, request, timeoutMs);
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

      if (isFileSizeResponse(message)) {
        this.#resolvePending(this.#fileSizePending, message);
        continue;
      }

      if (isAnalyzeDocumentResponse(message)) {
        if (this.#resolvePending(this.#documentPending, message)) {
          continue;
        }

        this.#resolvePending(this.#specifiersPending, message);
        continue;
      }

      if (isBatchResponse(message)) {
        const pending = this.#batchPending.get(message.request_id);

        if (!pending) {
          continue;
        }

        if (isStreamingPartial(message)) {
          pending.onPartial?.(message);
          this.emit("batchPartial", message);
          continue;
        }

        this.#batchPending.delete(message.request_id);
        pending.resolve(message);
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

    for (const pending of this.#batchPending.values()) {
      pending.reject(error);
    }

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

    for (const pending of this.#fileSizePending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#fileSizeDocumentPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#completionPending.values()) {
      pending.reject(error);
    }

    this.#batchPending.clear();
    this.#documentPending.clear();
    this.#packageJsonPending.clear();
    this.#specifiersPending.clear();
    this.#exportsPending.clear();
    this.#fileSizePending.clear();
    this.#fileSizeDocumentPending.clear();
    this.#completionPending.clear();

    if (emitDisconnect && !this.#disposed) {
      this.#logger?.warn(`IPC disconnected: ${error.message}`);
      this.emit("disconnect", error);
    }
  }
}

const isBatchResponse = (value: unknown): value is BatchResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<BatchResponse>;
  return (
    typeof candidate.version === "number" &&
    typeof candidate.request_id === "number" &&
    Array.isArray(candidate.imports) &&
    (candidate.indexes === undefined || candidate.indexes.every((index) => typeof index === "number"))
  );
};

const isStreamingPartial = (response: BatchResponse): boolean =>
  Array.isArray(response.indexes) && response.indexes.length > 0;

const isPackageJsonStreamingPartial = (response: AnalyzePackageJsonResponse): boolean =>
  Array.isArray(response.indexes) && response.indexes.length > 0;

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
    candidate.imports.every((item) =>
      !!item &&
      typeof item === "object" &&
      "detected" in item &&
      typeof (item as { status?: unknown }).status === "string")
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
    (candidate.indexes === undefined || candidate.indexes.every((index) => typeof index === "number")) &&
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

const isCompleteImportMembersResponse = (value: unknown): value is CompleteImportMembersResponse => {
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
