import { EventEmitter } from "node:events";
import net from "node:net";
import type {
  BatchRequest,
  BatchResponse,
  ClientMessage,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
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

interface PendingExportsRequest {
  resolve: (response: EnumerateExportsResponse) => void;
  reject: (error: Error) => void;
}

interface PendingFileSizeRequest {
  resolve: (response: FileSizeResponse) => void;
  reject: (error: Error) => void;
}

export class IpcClient extends EventEmitter {
  readonly #socket: net.Socket;
  readonly #decoder = new FrameDecoder();
  readonly #batchPending = new Map<number, PendingBatchRequest>();
  readonly #exportsPending = new Map<number, PendingExportsRequest>();
  readonly #fileSizePending = new Map<number, PendingFileSizeRequest>();
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

  requestExports(
    request: EnumerateExportsRequest,
    timeoutMs = 10000,
  ): Promise<EnumerateExportsResponse> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.#exportsPending.has(request.request_id)) {
          this.#exportsPending.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);

      this.#exportsPending.set(request.request_id, {
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

  requestFileSize(
    request: FileSizeRequest,
    timeoutMs = 10000,
  ): Promise<FileSizeResponse> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.#fileSizePending.has(request.request_id)) {
          this.#fileSizePending.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);

      this.#fileSizePending.set(request.request_id, {
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
      if (isFileSizeResponse(message)) {
        const pending = this.#fileSizePending.get(message.request_id);
        if (!pending) {
          continue;
        }

        this.#fileSizePending.delete(message.request_id);
        pending.resolve(message);
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

      const pending = this.#exportsPending.get(message.request_id);
      if (!pending) {
        continue;
      }

      this.#exportsPending.delete(message.request_id);
      pending.resolve(message);
    }
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

    for (const pending of this.#exportsPending.values()) {
      pending.reject(error);
    }

    for (const pending of this.#fileSizePending.values()) {
      pending.reject(error);
    }

    this.#batchPending.clear();
    this.#exportsPending.clear();
    this.#fileSizePending.clear();

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
