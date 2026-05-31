import { EventEmitter } from "node:events";
import net from "node:net";
import type { BatchRequest, BatchResponse, ClientMessage } from "./protocol.js";
import { FrameDecoder, encodeFrame } from "./codec.js";

interface PendingRequest {
  resolve: (response: BatchResponse) => void;
  reject: (error: Error) => void;
}

export class IpcClient extends EventEmitter {
  readonly #socket: net.Socket;
  readonly #decoder = new FrameDecoder();
  readonly #pending = new Map<number, PendingRequest>();
  #closed = false;
  #disposed = false;

  private constructor(socket: net.Socket) {
    super();
    this.#socket = socket;
    this.#socket.on("data", (chunk: Buffer) => this.#handleData(chunk));
    this.#socket.on("close", () => this.#handleClose(new Error("IPC socket closed")));
    this.#socket.on("error", (error) => this.#handleClose(error));
  }

  static connect(pipeName: string, timeoutMs = 2000): Promise<IpcClient> {
    const startedAt = Date.now();

    return new Promise((resolve, reject) => {
      const attempt = (): void => {
        const socket = net.createConnection(pipeName);
        let settled = false;

        socket.once("connect", () => {
          settled = true;
          resolve(new IpcClient(socket));
        });
        socket.once("error", (error) => {
          socket.destroy();

          if (settled) {
            return;
          }

          if (Date.now() - startedAt >= timeoutMs) {
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

  requestBatch(request: BatchRequest, timeoutMs = 10000): Promise<BatchResponse> {
    this.send(request);

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        if (this.#pending.has(request.request_id)) {
          this.#pending.delete(request.request_id);
          reject(new Error(`IPC request timed out after ${timeoutMs}ms`));
        }
      }, timeoutMs);

      this.#pending.set(request.request_id, {
        resolve: (response) => {
          clearTimeout(timer);
          resolve(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
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
      this.#handleClose(error instanceof Error ? error : new Error(String(error)));
      return;
    }

    for (const message of messages) {
      if (!isBatchResponse(message)) {
        continue;
      }

      const pending = this.#pending.get(message.request_id);

      if (!pending) {
        continue;
      }

      this.#pending.delete(message.request_id);
      pending.resolve(message);
    }
  }

  #handleClose(error: Error, emitDisconnect = true): void {
    if (this.#closed) {
      return;
    }

    this.#closed = true;
    this.#decoder.reset();

    for (const pending of this.#pending.values()) {
      pending.reject(error);
    }

    this.#pending.clear();

    if (emitDisconnect && !this.#disposed) {
      this.emit("disconnect", error);
    }
  }
}

const isBatchResponse = (value: unknown): value is BatchResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<BatchResponse>;
  return typeof candidate.version === "number" && typeof candidate.request_id === "number" && Array.isArray(candidate.imports);
};
