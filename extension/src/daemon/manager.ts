import type * as vscode from "vscode";
import type {
  AnalyzeDocumentRequest,
  AnalyzeDocumentResponse,
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  AnalyzeSpecifiersRequest,
  AnalyzeSpecifiersResponse,
  BatchRequest,
  BatchResponse,
  CompleteImportMembersRequest,
  CompleteImportMembersResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeDocumentRequest,
  FileSizeDocumentResponse,
  FileSizeRequest,
  FileSizeResponse,
} from "../ipc/protocol.js";
import type { ImportLensLogger } from "../logger.js";
import type { Logger } from "../logging/types.js";
import { NativeDaemonTransport } from "./nativeTransport.js";
import { TransportCoordinator, type DaemonState, type DaemonStateEvent } from "./transport.js";

export class DaemonManager implements vscode.Disposable {
  readonly #transport: TransportCoordinator;

  constructor(context: vscode.ExtensionContext, logger: ImportLensLogger) {
    this.#transport = new TransportCoordinator(
      [new NativeDaemonTransport(context, logger.child({ component: "daemon" }))],
      logger.child({ component: "transport" }),
    );
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

  analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse | null> {
    return this.#transport.analyzeDocument(request);
  }

  analyzePackageJson(
    request: AnalyzePackageJsonRequest,
    onPartial?: (response: AnalyzePackageJsonResponse) => void,
  ): Promise<AnalyzePackageJsonResponse | null> {
    return this.#transport.analyzePackageJson(request, onPartial);
  }

  analyzeSpecifiers(request: AnalyzeSpecifiersRequest): Promise<AnalyzeSpecifiersResponse | null> {
    return this.#transport.analyzeSpecifiers(request);
  }

  enumerateExports(request: EnumerateExportsRequest): Promise<EnumerateExportsResponse | null> {
    return this.#transport.enumerateExports(request);
  }

  requestFileSize(request: FileSizeRequest): Promise<FileSizeResponse | null> {
    return this.#transport.requestFileSize(request);
  }

  requestFileSizeDocument(request: FileSizeDocumentRequest): Promise<FileSizeDocumentResponse | null> {
    return this.#transport.requestFileSizeDocument(request);
  }

  completeImportMembers(request: CompleteImportMembersRequest): Promise<CompleteImportMembersResponse | null> {
    return this.#transport.completeImportMembers(request);
  }

  invalidatePackage(packageName: string): void {
    this.#transport.invalidatePackage(packageName);
  }

  invalidateAll(): void {
    this.#transport.invalidateAll();
  }

  nodeModulesChanged(packageJsonPaths: readonly string[]): void {
    this.#transport.nodeModulesChanged(packageJsonPaths);
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
