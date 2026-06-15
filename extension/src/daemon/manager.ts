import type * as vscode from "vscode";
import type {
  BatchRequest,
  BatchResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeRequest,
  FileSizeResponse,
} from "../ipc/protocol.js";
import type { ImportLensLogger } from "../logger.js";
import { NativeDaemonTransport } from "./nativeTransport.js";
import { TransportCoordinator, type DaemonState, type DaemonStateEvent } from "./transport.js";

export class DaemonManager implements vscode.Disposable {
  readonly #transport: TransportCoordinator;

  constructor(context: vscode.ExtensionContext, logger: ImportLensLogger) {
    this.#transport = new TransportCoordinator([
      new NativeDaemonTransport(context, logger),
    ]);
  }

  get state(): DaemonState {
    return this.#transport.state;
  }

  get onDidChangeState(): DaemonStateEvent {
    return this.#transport.onDidChangeState;
  }

  start(analysisRoot?: string): Promise<DaemonState> {
    return this.#transport.start(analysisRoot);
  }

  sendBatch(request: BatchRequest, onPartial?: (response: BatchResponse) => void): Promise<BatchResponse | null> {
    return this.#transport.sendBatch(request, onPartial);
  }

  enumerateExports(request: EnumerateExportsRequest): Promise<EnumerateExportsResponse | null> {
    return this.#transport.enumerateExports(request);
  }

  requestFileSize(request: FileSizeRequest): Promise<FileSizeResponse | null> {
    return this.#transport.requestFileSize(request);
  }

  invalidatePackage(packageName: string): void {
    this.#transport.invalidatePackage(packageName);
  }

  invalidateAll(): void {
    this.#transport.invalidateAll();
  }

  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void {
    this.#transport.prewarmPackageJson(packageJsonPath, activeDocumentPath);
  }

  dispose(): Promise<void> {
    return this.#transport.shutdown();
  }

  restart(analysisRoot?: string): Promise<DaemonState> {
    return this.dispose().then(() => this.start(analysisRoot));
  }
}
