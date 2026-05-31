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
  #state: DaemonState = "unavailable";

  constructor(startState: DaemonState) {
    this.#startState = startState;
  }

  get state(): DaemonState {
    return this.#state;
  }

  async start(): Promise<DaemonState> {
    this.calls.push("start");
    this.#state = this.#startState;
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
    this.#state = "unavailable";
  }

  dispose(): void {
    this.calls.push("dispose");
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
