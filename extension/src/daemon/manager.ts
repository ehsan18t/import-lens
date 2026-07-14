import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
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
  RefreshRegistryHintsRequest,
  RefreshRegistryHintsResponse,
  WorkspaceReportRequest,
  WorkspaceReportResponse,
} from "../ipc/protocol.js";
import type { ImportLensLogger } from "../logger.js";
import { NativeDaemonTransport } from "./nativeTransport.js";
import {
  type DaemonRefreshedResultsEvent,
  type DaemonState,
  type DaemonStateEvent,
  TransportCoordinator,
} from "./transport.js";

export class DaemonManager implements vscode.Disposable {
  readonly #transport: TransportCoordinator;

  constructor(context: vscode.ExtensionContext, logger: ImportLensLogger) {
    this.#transport = new TransportCoordinator(
      [
        new NativeDaemonTransport(
          context,
          logger.child({ component: "daemon" }),
          () => vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
          getImportLensConfig,
        ),
      ],
      logger.child({ component: "transport" }),
    );
  }

  get state(): DaemonState {
    return this.#transport.state;
  }

  get onDidChangeState(): DaemonStateEvent {
    return this.#transport.onDidChangeState;
  }

  get onRefreshedResults(): DaemonRefreshedResultsEvent {
    return this.#transport.onRefreshedResults;
  }

  start(analysisRoot?: string): Promise<DaemonState> {
    return this.#transport.start(analysisRoot);
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

  requestFileSizeDocument(
    request: FileSizeDocumentRequest,
  ): Promise<FileSizeDocumentResponse | null> {
    return this.#transport.requestFileSizeDocument(request);
  }

  completeImportMembers(
    request: CompleteImportMembersRequest,
  ): Promise<CompleteImportMembersResponse | null> {
    return this.#transport.completeImportMembers(request);
  }

  cacheStatus(request: CacheStatusRequest): Promise<CacheStatusResponse | null> {
    return this.#transport.cacheStatus(request);
  }

  listCache(request: CacheListRequest): Promise<CacheListResponse | null> {
    return this.#transport.listCache(request);
  }

  removeCache(request: CacheRemoveRequest): Promise<CacheRemoveResponse | null> {
    return this.#transport.removeCache(request);
  }

  refreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse | null> {
    return this.#transport.refreshRegistryHints(request, onPartial);
  }

  requestWorkspaceReport(request: WorkspaceReportRequest): Promise<WorkspaceReportResponse | null> {
    return this.#transport.requestWorkspaceReport(request);
  }

  invalidatePackage(packageName: string): void {
    this.#transport.invalidatePackage(packageName);
  }

  invalidateAll(): void {
    this.#transport.invalidateAll();
  }

  nodeModulesChanged(
    packageJsonPaths: readonly string[],
    tsconfigPaths: readonly string[] = [],
  ): void {
    this.#transport.nodeModulesChanged(packageJsonPaths, tsconfigPaths);
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
