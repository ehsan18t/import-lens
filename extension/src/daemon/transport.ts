import type {
  AnalyzeDocumentRequest,
  AnalyzeDocumentResponse,
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  AnalyzeSpecifiersRequest,
  AnalyzeSpecifiersResponse,
  CacheCleanupRequest,
  CacheCleanupResponse,
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

import type { Logger } from "../logging/types.js";

export type DaemonState = "ready" | "unavailable";
export type DaemonStateEvent = (listener: (state: DaemonState) => void) => { dispose(): void };

export interface AnalysisTransport {
  readonly state: DaemonState;
  readonly onDidChangeState?: DaemonStateEvent;
  start(analysisRoot?: string): Promise<DaemonState>;
  analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse | null>;
  analyzePackageJson(
    request: AnalyzePackageJsonRequest,
    onPartial?: (response: AnalyzePackageJsonResponse) => void,
  ): Promise<AnalyzePackageJsonResponse | null>;
  analyzeSpecifiers(request: AnalyzeSpecifiersRequest): Promise<AnalyzeSpecifiersResponse | null>;
  enumerateExports(request: EnumerateExportsRequest): Promise<EnumerateExportsResponse | null>;
  requestFileSizeDocument(
    request: FileSizeDocumentRequest,
  ): Promise<FileSizeDocumentResponse | null>;
  completeImportMembers(
    request: CompleteImportMembersRequest,
  ): Promise<CompleteImportMembersResponse | null>;
  cacheStatus(request: CacheStatusRequest): Promise<CacheStatusResponse | null>;
  cleanupCache(request: CacheCleanupRequest): Promise<CacheCleanupResponse | null>;
  listCache(request: CacheListRequest): Promise<CacheListResponse | null>;
  removeCache(request: CacheRemoveRequest): Promise<CacheRemoveResponse | null>;
  refreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse | null>;
  requestWorkspaceReport(request: WorkspaceReportRequest): Promise<WorkspaceReportResponse | null>;
  invalidatePackage(packageName: string): void;
  invalidateAll(): void;
  nodeModulesChanged(packageJsonPaths: readonly string[]): void;
  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void;
  shutdown(): Promise<void>;
  dispose(): void | Promise<void>;
}

export class TransportCoordinator implements AnalysisTransport {
  readonly #transports: readonly AnalysisTransport[];
  readonly #stateListeners = new Set<(state: DaemonState) => void>();
  readonly #logger?: Pick<Logger, "debug" | "info">;
  #activeTransport: AnalysisTransport | null = null;
  #startPromise: Promise<DaemonState> | null = null;
  #state: DaemonState = "unavailable";

  constructor(transports: readonly AnalysisTransport[], logger?: Pick<Logger, "debug" | "info">) {
    this.#transports = transports;
    this.#logger = logger;

    for (const transport of transports) {
      transport.onDidChangeState?.((state) => this.#handleTransportState(transport, state));
    }
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

  async start(analysisRoot?: string): Promise<DaemonState> {
    if (this.#state === "ready" && this.#activeTransport?.state === "ready") {
      return "ready";
    }

    if (this.#startPromise) {
      this.#logger?.debug("Coalescing concurrent daemon startup attempt.");
      return this.#startPromise;
    }

    this.#startPromise = this.#startTransports(analysisRoot).finally(() => {
      this.#startPromise = null;
    });

    return this.#startPromise;
  }

  async #startTransports(analysisRoot?: string): Promise<DaemonState> {
    for (const transport of this.#transports) {
      const state = await transport.start(analysisRoot);

      if (state === "ready") {
        this.#activeTransport = transport;
        this.#logger?.info("Daemon transport is ready.");
        this.#setState("ready");
        return this.#state;
      }
    }

    this.#activeTransport = null;
    this.#logger?.info("All daemon transports unavailable.");
    this.#setState("unavailable");
    return this.#state;
  }

  analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse | null> {
    return this.#activeTransport?.analyzeDocument(request) ?? Promise.resolve(null);
  }

  analyzePackageJson(
    request: AnalyzePackageJsonRequest,
    onPartial?: (response: AnalyzePackageJsonResponse) => void,
  ): Promise<AnalyzePackageJsonResponse | null> {
    return this.#activeTransport?.analyzePackageJson(request, onPartial) ?? Promise.resolve(null);
  }

  analyzeSpecifiers(request: AnalyzeSpecifiersRequest): Promise<AnalyzeSpecifiersResponse | null> {
    return this.#activeTransport?.analyzeSpecifiers(request) ?? Promise.resolve(null);
  }

  enumerateExports(request: EnumerateExportsRequest): Promise<EnumerateExportsResponse | null> {
    return this.#activeTransport?.enumerateExports(request) ?? Promise.resolve(null);
  }

  requestFileSizeDocument(
    request: FileSizeDocumentRequest,
  ): Promise<FileSizeDocumentResponse | null> {
    return this.#activeTransport?.requestFileSizeDocument(request) ?? Promise.resolve(null);
  }

  completeImportMembers(
    request: CompleteImportMembersRequest,
  ): Promise<CompleteImportMembersResponse | null> {
    return this.#activeTransport?.completeImportMembers(request) ?? Promise.resolve(null);
  }

  cacheStatus(request: CacheStatusRequest): Promise<CacheStatusResponse | null> {
    return this.#activeTransport?.cacheStatus(request) ?? Promise.resolve(null);
  }

  cleanupCache(request: CacheCleanupRequest): Promise<CacheCleanupResponse | null> {
    return this.#activeTransport?.cleanupCache(request) ?? Promise.resolve(null);
  }

  listCache(request: CacheListRequest): Promise<CacheListResponse | null> {
    return this.#activeTransport?.listCache(request) ?? Promise.resolve(null);
  }

  removeCache(request: CacheRemoveRequest): Promise<CacheRemoveResponse | null> {
    return this.#activeTransport?.removeCache(request) ?? Promise.resolve(null);
  }

  refreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse | null> {
    return this.#activeTransport?.refreshRegistryHints(request, onPartial) ?? Promise.resolve(null);
  }

  requestWorkspaceReport(request: WorkspaceReportRequest): Promise<WorkspaceReportResponse | null> {
    return this.#activeTransport?.requestWorkspaceReport(request) ?? Promise.resolve(null);
  }

  invalidatePackage(packageName: string): void {
    this.#activeTransport?.invalidatePackage(packageName);
  }

  invalidateAll(): void {
    this.#activeTransport?.invalidateAll();
  }

  nodeModulesChanged(packageJsonPaths: readonly string[]): void {
    this.#activeTransport?.nodeModulesChanged(packageJsonPaths);
  }

  prewarmPackageJson(packageJsonPath: string, activeDocumentPath: string): void {
    this.#activeTransport?.prewarmPackageJson(packageJsonPath, activeDocumentPath);
  }

  async shutdown(): Promise<void> {
    await Promise.all(this.#transports.map((transport) => transport.shutdown()));
    this.#activeTransport = null;
    this.#startPromise = null;
    this.#setState("unavailable");
  }

  dispose(): void {
    void this.shutdown();
  }

  #handleTransportState(transport: AnalysisTransport, state: DaemonState): void {
    if (
      transport !== this.#activeTransport &&
      !(state === "ready" && this.#activeTransport === null)
    ) {
      return;
    }

    if (state === "ready") {
      this.#activeTransport = transport;
    }

    this.#setState(state);
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
}
