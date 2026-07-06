import assert from "node:assert/strict";
import test from "node:test";
import {
  type AnalysisTransport,
  type DaemonState,
  TransportCoordinator,
} from "../../src/daemon/transport.js";
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
} from "../../src/ipc/protocol.js";
import { protocolVersion } from "../../src/ipc/protocol.js";

class FakeTransport implements AnalysisTransport {
  readonly #startState: DaemonState;
  readonly calls: string[] = [];
  readonly #stateListeners = new Set<(state: DaemonState) => void>();
  #state: DaemonState = "unavailable";

  constructor(startState: DaemonState) {
    this.#startState = startState;
  }

  get state(): DaemonState {
    return this.#state;
  }

  readonly onDidChangeState = (listener: (state: DaemonState) => void): { dispose(): void } => {
    this.#stateListeners.add(listener);

    return {
      dispose: () => {
        this.#stateListeners.delete(listener);
      },
    };
  };

  setState(state: DaemonState): void {
    this.#state = state;

    for (const listener of this.#stateListeners) {
      listener(state);
    }
  }

  async start(analysisRoot?: string): Promise<DaemonState> {
    this.calls.push(analysisRoot ? `start:${analysisRoot}` : "start");
    this.setState(this.#startState);
    return this.#state;
  }

  async analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse> {
    this.calls.push(`document:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      imports: [],
      error: null,
      diagnostics: [],
    };
  }

  async analyzePackageJson(
    request: AnalyzePackageJsonRequest,
  ): Promise<AnalyzePackageJsonResponse> {
    this.calls.push(`packageJson:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      sections: [],
      states: [],
      error: null,
      diagnostics: [],
    };
  }

  async analyzeSpecifiers(request: AnalyzeSpecifiersRequest): Promise<AnalyzeSpecifiersResponse> {
    this.calls.push(`specifiers:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      imports: [],
      error: null,
      diagnostics: [],
    };
  }

  async enumerateExports(request: EnumerateExportsRequest): Promise<EnumerateExportsResponse> {
    this.calls.push(`exports:${request.request_id}:${request.specifier}`);
    return {
      version: request.version,
      request_id: request.request_id,
      specifier: request.specifier,
      exports: ["alpha"],
      error: null,
      diagnostics: [],
    };
  }

  async requestFileSizeDocument(
    request: FileSizeDocumentRequest,
  ): Promise<FileSizeDocumentResponse> {
    this.calls.push(`fileSizeDocument:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      raw_bytes: 10,
      minified_bytes: 8,
      gzip_bytes: 6,
      brotli_bytes: 5,
      zstd_bytes: 6,
      imports: [],
      states: [],
      error: null,
      diagnostics: [],
    };
  }

  async completeImportMembers(
    request: CompleteImportMembersRequest,
  ): Promise<CompleteImportMembersResponse> {
    this.calls.push(`completion:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      specifier: null,
      exports: [],
      imported_names: [],
      error: null,
      diagnostics: [],
    };
  }

  async cacheStatus(request: CacheStatusRequest): Promise<CacheStatusResponse> {
    this.calls.push(`cacheStatus:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      total_size_bytes: 2048,
      project_count: 1,
      max_size_mb: 512,
      current_project: null,
      error: null,
      diagnostics: [],
    };
  }

  async listCache(request: CacheListRequest): Promise<CacheListResponse> {
    this.calls.push(`listCache:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      shards: [],
      error: null,
      diagnostics: [],
    };
  }

  async removeCache(request: CacheRemoveRequest): Promise<CacheRemoveResponse> {
    this.calls.push(`removeCache:${request.request_id}:${request.scope}`);
    return {
      version: request.version,
      request_id: request.request_id,
      removed: [],
      failed: [],
      error: null,
      diagnostics: [],
    };
  }

  async refreshRegistryHints(
    request: RefreshRegistryHintsRequest,
    onPartial?: (response: RefreshRegistryHintsResponse) => void,
  ): Promise<RefreshRegistryHintsResponse> {
    this.calls.push(`registryHints:${request.request_id}`);
    const partial: RefreshRegistryHintsResponse = {
      version: request.version,
      request_id: request.request_id,
      indexes: [0],
      results: [
        {
          target: request.targets[0],
          hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
          error: null,
        },
      ],
      error: null,
      diagnostics: [],
    };
    onPartial?.(partial);
    return {
      ...partial,
      indexes: undefined,
    };
  }

  async requestWorkspaceReport(request: WorkspaceReportRequest): Promise<WorkspaceReportResponse> {
    this.calls.push(`workspaceReport:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      rows: [],
      summary: {
        importCount: 0,
        totalBrotliBytes: 0,
        lowConfidenceCount: 0,
        mediumConfidenceCount: 0,
        conservativeCount: 0,
        budgetViolationCount: 0,
        duplicateImports: [],
        sharedModules: [],
        treemap: [],
      },
      error: null,
      diagnostics: [],
    };
  }

  invalidatePackage(packageName: string): void {
    this.calls.push(`invalidate:${packageName}`);
  }

  invalidateAll(): void {
    this.calls.push("invalidateAll");
  }

  nodeModulesChanged(packageJsonPaths: readonly string[]): void {
    this.calls.push(`nodeModules:${packageJsonPaths.length}`);
  }

  prewarmPackageJson(packageJsonPath: string): void {
    this.calls.push(`prewarm:${packageJsonPath}`);
  }

  async shutdown(): Promise<void> {
    this.calls.push("shutdown");
    this.setState("unavailable");
  }

  dispose(): void {
    this.calls.push("dispose");
  }
}

class SlowReadyTransport implements AnalysisTransport {
  readonly calls: string[] = [];
  #state: DaemonState = "unavailable";
  #releaseStart: (() => void) | undefined;
  #startGate: Promise<void> | undefined;

  get state(): DaemonState {
    return this.#state;
  }

  async start(analysisRoot?: string): Promise<DaemonState> {
    this.calls.push(analysisRoot ? `start:${analysisRoot}` : "start");
    this.#startGate ??= new Promise((resolve) => {
      this.#releaseStart = resolve;
    });

    await this.#startGate;
    this.#state = "ready";
    return this.#state;
  }

  releaseStart(): void {
    this.#releaseStart?.();
  }

  async analyzeDocument(): Promise<AnalyzeDocumentResponse | null> {
    return null;
  }

  async analyzePackageJson(): Promise<AnalyzePackageJsonResponse | null> {
    return null;
  }

  async analyzeSpecifiers(): Promise<AnalyzeSpecifiersResponse | null> {
    return null;
  }

  async enumerateExports(): Promise<EnumerateExportsResponse | null> {
    return null;
  }

  async requestFileSizeDocument(): Promise<FileSizeDocumentResponse | null> {
    return null;
  }

  async completeImportMembers(): Promise<CompleteImportMembersResponse | null> {
    return null;
  }

  async cacheStatus(): Promise<CacheStatusResponse | null> {
    return null;
  }

  async listCache(): Promise<CacheListResponse | null> {
    return null;
  }

  async removeCache(): Promise<CacheRemoveResponse | null> {
    return null;
  }

  async refreshRegistryHints(): Promise<RefreshRegistryHintsResponse | null> {
    return null;
  }

  async requestWorkspaceReport(): Promise<WorkspaceReportResponse | null> {
    return null;
  }

  invalidatePackage(): void {}

  invalidateAll(): void {}

  nodeModulesChanged(): void {}

  prewarmPackageJson(): void {}

  async shutdown(): Promise<void> {
    this.#state = "unavailable";
  }

  dispose(): void {}
}

test("TransportCoordinator selects the first ready transport and delegates requests", async () => {
  const unavailable = new FakeTransport("unavailable");
  const ready = new FakeTransport("ready");
  const coordinator = new TransportCoordinator([unavailable, ready]);

  assert.equal(await coordinator.start(), "ready");
  await coordinator.enumerateExports(exportsRequest(8));
  await coordinator.cacheStatus(cacheStatusRequest(10));
  await coordinator.listCache(cacheListRequest(12));
  await coordinator.removeCache(cacheRemoveRequest(13));
  coordinator.invalidatePackage("react");
  coordinator.prewarmPackageJson("/workspace/package.json", "/workspace/package.json");

  assert.deepEqual(unavailable.calls, ["start"]);
  assert.deepEqual(ready.calls, [
    "start",
    "exports:8:tiny-lib",
    "cacheStatus:10",
    "listCache:12",
    "removeCache:13:current_project",
    "invalidate:react",
    "prewarm:/workspace/package.json",
  ]);
});

test("TransportCoordinator returns null when no transport is ready", async () => {
  const coordinator = new TransportCoordinator([new FakeTransport("unavailable")]);

  assert.equal(await coordinator.start(), "unavailable");
  assert.equal(await coordinator.enumerateExports(exportsRequest(1)), null);
});

test("TransportCoordinator passes analysis root to transport startup", async () => {
  const ready = new FakeTransport("ready");
  const coordinator = new TransportCoordinator([ready]);

  assert.equal(await coordinator.start("/workspace/loose-app"), "ready");
  assert.deepEqual(ready.calls, ["start:/workspace/loose-app"]);
});

test("TransportCoordinator coalesces concurrent startup attempts", async () => {
  const ready = new SlowReadyTransport();
  const coordinator = new TransportCoordinator([ready]);

  const first = coordinator.start("/workspace/app");
  const second = coordinator.start("/workspace/app");
  ready.releaseStart();

  assert.deepEqual(await Promise.all([first, second]), ["ready", "ready"]);
  assert.deepEqual(ready.calls, ["start:/workspace/app"]);
});

test("TransportCoordinator shuts down all transports", async () => {
  const first = new FakeTransport("ready");
  const second = new FakeTransport("unavailable");
  const coordinator = new TransportCoordinator([first, second]);

  await coordinator.start();
  await coordinator.shutdown();

  assert.equal(coordinator.state, "unavailable");
  assert.equal(first.calls.includes("shutdown"), true);
  assert.equal(second.calls.includes("shutdown"), true);
});

test("TransportCoordinator emits active transport state changes", async () => {
  const ready = new FakeTransport("ready");
  const coordinator = new TransportCoordinator([ready]);
  const states: DaemonState[] = [];
  const subscription = coordinator.onDidChangeState((state) => states.push(state));

  await coordinator.start();
  ready.setState("unavailable");
  ready.setState("ready");
  await coordinator.shutdown();
  subscription.dispose();

  assert.deepEqual(states, ["ready", "unavailable", "ready", "unavailable"]);
  assert.equal(coordinator.state, "unavailable");
});

test("TransportCoordinator forwards registry refresh partial callbacks", async () => {
  const transport = new FakeTransport("ready");
  const coordinator = new TransportCoordinator([transport]);
  const partials: RefreshRegistryHintsResponse[] = [];

  await coordinator.start("/workspace");
  const response = await coordinator.refreshRegistryHints(
    {
      type: "refresh_registry_hints",
      version: protocolVersion,
      request_id: 88,
      targets: [{ name: "react", installedVersion: "18.2.0" }],
      mode: "refresh_stale",
    },
    (partial) => partials.push(partial),
  );

  assert.deepEqual(transport.calls, ["start:/workspace", "registryHints:88"]);
  assert.equal(partials.length, 1);
  assert.equal(response?.results[0]?.hint?.latestVersion, "19.0.0");
});

const exportsRequest = (requestId: number): EnumerateExportsRequest => ({
  type: "enumerate_exports",
  version: 2,
  request_id: requestId,
  workspace_root: "/workspace",
  active_document_path: "/workspace/src/app.ts",
  specifier: "tiny-lib",
  package: "tiny-lib",
  package_version: "1.0.0",
});

const cacheStatusRequest = (requestId: number): CacheStatusRequest => ({
  type: "cache_status",
  version: 6,
  request_id: requestId,
  workspace_root: "/workspace",
});

const cacheListRequest = (requestId: number): CacheListRequest => ({
  type: "cache_list",
  version: 6,
  request_id: requestId,
});

const cacheRemoveRequest = (requestId: number): CacheRemoveRequest => ({
  type: "cache_remove",
  version: 6,
  request_id: requestId,
  scope: "current_project",
  workspace_root: "/workspace",
});
