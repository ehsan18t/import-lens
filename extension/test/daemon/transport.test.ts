import assert from "node:assert/strict";
import test from "node:test";
import type {
  BatchRequest,
  BatchResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeRequest,
  FileSizeResponse,
} from "../../src/ipc/protocol.js";
import { TransportCoordinator, type AnalysisTransport, type DaemonState } from "../../src/daemon/transport.js";

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

  async sendBatch(request: BatchRequest): Promise<BatchResponse> {
    this.calls.push(`batch:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      imports: [],
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

  async requestFileSize(request: FileSizeRequest): Promise<FileSizeResponse> {
    this.calls.push(`fileSize:${request.request_id}`);
    return {
      version: request.version,
      request_id: request.request_id,
      raw_bytes: 10,
      minified_bytes: 8,
      gzip_bytes: 6,
      brotli_bytes: 5,
      zstd_bytes: 6,
      imports: [],
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

  async sendBatch(): Promise<BatchResponse | null> {
    return null;
  }

  async enumerateExports(): Promise<EnumerateExportsResponse | null> {
    return null;
  }

  async requestFileSize(): Promise<FileSizeResponse | null> {
    return null;
  }

  invalidatePackage(): void {
    return undefined;
  }

  invalidateAll(): void {
    return undefined;
  }

  prewarmPackageJson(): void {
    return undefined;
  }

  async shutdown(): Promise<void> {
    this.#state = "unavailable";
  }

  dispose(): void {
    return undefined;
  }
}

test("TransportCoordinator selects the first ready transport and delegates requests", async () => {
  const unavailable = new FakeTransport("unavailable");
  const ready = new FakeTransport("ready");
  const coordinator = new TransportCoordinator([unavailable, ready]);

  assert.equal(await coordinator.start(), "ready");
  await coordinator.sendBatch(batch(7));
  await coordinator.enumerateExports(exportsRequest(8));
  await coordinator.requestFileSize(fileSizeRequest(9));
  coordinator.invalidatePackage("react");
  coordinator.prewarmPackageJson("/workspace/package.json", "/workspace/package.json");

  assert.deepEqual(unavailable.calls, ["start"]);
  assert.deepEqual(ready.calls, [
    "start",
    "batch:7",
    "exports:8:tiny-lib",
    "fileSize:9",
    "invalidate:react",
    "prewarm:/workspace/package.json",
  ]);
});

test("TransportCoordinator returns null when no transport is ready", async () => {
  const coordinator = new TransportCoordinator([new FakeTransport("unavailable")]);

  assert.equal(await coordinator.start(), "unavailable");
  assert.equal(await coordinator.sendBatch(batch(1)), null);
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

const batch = (requestId: number): BatchRequest => ({
  version: 1,
  request_id: requestId,
  workspace_root: "/workspace",
  active_document_path: "/workspace/src/app.ts",
  imports: [],
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

const fileSizeRequest = (requestId: number): FileSizeRequest => ({
  type: "file_size",
  version: 2,
  request_id: requestId,
  workspace_root: "/workspace",
  active_document_path: "/workspace/src/app.ts",
  imports: [],
});
