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

  requestBatch(request: BatchRequest): Promise<BatchResponse> {
    this.send(request);

    return new Promise((resolve, reject) => {
      this.#pending.set(request.request_id, { resolve, reject });
    });
  }

  dispose(): void {
    this.#socket.destroy();
    this.#handleClose(new Error("IPC client disposed"));
  }

  #handleData(chunk: Buffer): void {
    for (const message of this.#decoder.push(chunk)) {
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

  #handleClose(error: Error): void {
    this.#decoder.reset();

    for (const pending of this.#pending.values()) {
      pending.reject(error);
    }

    this.#pending.clear();
    this.emit("disconnect", error);
  }
}

const isBatchResponse = (value: unknown): value is BatchResponse => {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<BatchResponse>;
  return typeof candidate.version === "number" && typeof candidate.request_id === "number" && Array.isArray(candidate.imports);
};

