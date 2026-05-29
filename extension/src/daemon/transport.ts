import type { BatchRequest, BatchResponse } from "../ipc/protocol.js";

export type DaemonState = "ready" | "unavailable";

export interface AnalysisTransport {
  readonly state: DaemonState;
  start(): Promise<DaemonState>;
  sendBatch(request: BatchRequest): Promise<BatchResponse | null>;
  invalidatePackage(packageName: string): void;
  invalidateAll(): void;
  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void;
  shutdown(): Promise<void>;
  dispose(): void | Promise<void>;
}

export class TransportCoordinator implements AnalysisTransport {
  readonly #transports: readonly AnalysisTransport[];
  #activeTransport: AnalysisTransport | null = null;
  #state: DaemonState = "unavailable";

  constructor(transports: readonly AnalysisTransport[]) {
    this.#transports = transports;
  }

  get state(): DaemonState {
    return this.#state;
  }

  async start(): Promise<DaemonState> {
    for (const transport of this.#transports) {
      const state = await transport.start();

      if (state === "ready") {
        this.#activeTransport = transport;
        this.#state = "ready";
        return this.#state;
      }
    }

    this.#activeTransport = null;
    this.#state = "unavailable";
    return this.#state;
  }

  sendBatch(request: BatchRequest): Promise<BatchResponse | null> {
    return this.#activeTransport?.sendBatch(request) ?? Promise.resolve(null);
  }

  invalidatePackage(packageName: string): void {
    this.#activeTransport?.invalidatePackage(packageName);
  }

  invalidateAll(): void {
    this.#activeTransport?.invalidateAll();
  }

  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void {
    this.#activeTransport?.prewarmPackageJson(packageJsonPath, activeDocumentPath);
  }

  async shutdown(): Promise<void> {
    await Promise.all(this.#transports.map((transport) => transport.shutdown()));
    this.#activeTransport = null;
    this.#state = "unavailable";
  }

  dispose(): void {
    void this.shutdown();
  }
}
