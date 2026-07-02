import assert from "node:assert/strict";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { IpcClient } from "../../src/ipc/client.js";
import { FrameDecoder, encodeFrame } from "../../src/ipc/codec.js";
import type {
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  BatchRequest,
  BatchResponse,
  CacheCleanupRequest,
  CacheCleanupResponse,
  CacheListRequest,
  CacheListResponse,
  CacheRemoveRequest,
  CacheRemoveResponse,
  CacheStatusRequest,
  CacheStatusResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeRequest,
  FileSizeResponse,
  ImportResult,
  PackageJsonDependencyAnalysisItem,
  PackageJsonDependencyEntry,
  RefreshRegistryHintsRequest,
  RefreshRegistryHintsResponse,
  WorkspaceReportRequest,
  WorkspaceReportResponse,
} from "../../src/ipc/protocol.js";
import { protocolVersion } from "../../src/ipc/protocol.js";

const testPipeName = (): string => {
  const unique = `import-lens-ipc-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;

  if (process.platform === "win32") {
    return `\\\\.\\pipe\\${unique}`;
  }

  return path.join(tmpdir(), `${unique}.sock`);
};

const listen = async (server: net.Server, pipeName: string): Promise<void> =>
  new Promise((resolve, reject) => {
    const onError = (error: Error): void => {
      reject(error);
    };

    server.once("error", onError);
    server.listen(pipeName, () => {
      server.off("error", onError);
      resolve();
    });
  });

const closeServer = async (server: net.Server): Promise<void> =>
  new Promise((resolve, reject) => {
    server.close((error) => {
      if (error) {
        reject(error);
        return;
      }

      resolve();
    });
  });

const destroySockets = (sockets: Set<net.Socket>): void => {
  for (const socket of sockets) {
    socket.destroy();
  }
};

const emptyResult = (specifier: string): ImportResult => ({
  specifier,
  raw_bytes: 1,
  minified_bytes: 1,
  gzip_bytes: 1,
  brotli_bytes: 1,
  zstd_bytes: 1,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
});

const batchRequest = (requestId: number): BatchRequest => ({
  version: 2,
  request_id: requestId,
  workspace_root: "/workspace",
  active_document_path: "/workspace/src/app.ts",
  imports: [],
  streaming: true,
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

const cacheStatusRequest = (requestId: number): CacheStatusRequest => ({
  type: "cache_status",
  version: 6,
  request_id: requestId,
  workspace_root: "/workspace",
});

const cacheCleanupRequest = (requestId: number): CacheCleanupRequest => ({
  type: "cache_cleanup",
  version: 6,
  request_id: requestId,
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

const packageJsonRequest = (requestId: number): AnalyzePackageJsonRequest => ({
  type: "analyze_package_json",
  version: 5,
  request_id: requestId,
  workspace_root: "/workspace",
  active_document_path: "/workspace/package.json",
  source: "{\"dependencies\":{\"react\":\"^19.0.0\"}}",
  include_registry_hints: false,
  streaming: true,
});

const packageJsonEntry = (name: string): PackageJsonDependencyEntry => ({
  name,
  version: "^1.0.0",
  section: "dependencies",
  range: {
    start: { line: 1, character: 2 },
    end: { line: 1, character: 20 },
  },
  nameRange: {
    start: { line: 1, character: 2 },
    end: { line: 1, character: 9 },
  },
  valueRange: {
    start: { line: 1, character: 12 },
    end: { line: 1, character: 20 },
  },
});

const packageJsonState = (
  name: string,
  status: PackageJsonDependencyAnalysisItem["status"],
): PackageJsonDependencyAnalysisItem => ({
  entry: packageJsonEntry(name),
  name,
  section: "dependencies",
  status,
  installedVersion: "1.0.0",
  result: status === "ready" ? emptyResult(name) : undefined,
});

const registryRefreshRequest = (requestId: number): RefreshRegistryHintsRequest => ({
  type: "refresh_registry_hints",
  version: protocolVersion,
  request_id: requestId,
  targets: [{ name: "react", installedVersion: "18.2.0" }],
  mode: "refresh_stale",
});

test("IpcClient.dispose does not emit disconnect for intentional disposal", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    let disconnects = 0;

    client.on("disconnect", () => {
      disconnects++;
    });

    client.dispose();
    await delay(20);

    assert.equal(disconnects, 0);
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient emits one disconnect for external socket closure", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  let resolveAcceptedSocket: (socket: net.Socket) => void;
  const acceptedSocket = new Promise<net.Socket>((resolve) => {
    resolveAcceptedSocket = resolve;
  });
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    resolveAcceptedSocket(socket);
    socket.resume();
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    let disconnects = 0;
    const disconnected = new Promise<void>((resolve) => {
      client.on("disconnect", () => {
        disconnects++;
        resolve();
      });
    });

    (await acceptedSocket).destroy();
    await disconnected;
    await delay(20);

    assert.equal(disconnects, 1);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient emits streaming partials and resolves final batch response", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const partial: BatchResponse = {
    version: 2,
    request_id: 99,
    imports: [emptyResult("react")],
    indexes: [0],
  };
  const final: BatchResponse = {
    version: 2,
    request_id: 99,
    imports: [emptyResult("react")],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(partial));
      socket.write(encodeFrame(final));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const partials: BatchResponse[] = [];
    client.on("batchPartial", (response: BatchResponse) => {
      partials.push(response);
    });

    const response = await client.requestBatch(batchRequest(99));

    assert.equal(response.indexes, undefined);
    assert.equal(response.imports.length, 1);
    assert.equal(partials.length, 1);
    assert.deepEqual(partials[0]?.indexes, [0]);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient ignores stale streaming partials for other request IDs", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const stalePartial: BatchResponse = {
    version: 2,
    request_id: 98,
    imports: [emptyResult("stale-lib")],
    indexes: [0],
  };
  const final: BatchResponse = {
    version: 2,
    request_id: 99,
    imports: [emptyResult("react")],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(stalePartial));
      socket.write(encodeFrame(final));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const partials: BatchResponse[] = [];
    client.on("batchPartial", (response: BatchResponse) => {
      partials.push(response);
    });

    const response = await client.requestBatch(batchRequest(99));

    assert.equal(response.request_id, 99);
    assert.equal(response.imports.length, 1);
    assert.equal(partials.length, 0);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient emits package.json streaming partials and resolves final response", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const partial: AnalyzePackageJsonResponse = {
    version: 5,
    request_id: 199,
    sections: [],
    states: [packageJsonState("react", "loading")],
    indexes: [0],
    error: null,
    diagnostics: [],
  };
  const final: AnalyzePackageJsonResponse = {
    version: 5,
    request_id: 199,
    sections: [],
    states: [packageJsonState("react", "ready")],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(partial));
      socket.write(encodeFrame(final));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const partials: AnalyzePackageJsonResponse[] = [];
    client.on("packageJsonPartial", (response: AnalyzePackageJsonResponse) => {
      partials.push(response);
    });

    const response = await client.requestAnalyzePackageJson(packageJsonRequest(199));

    assert.equal(response.indexes, undefined);
    assert.equal(response.states[0]?.status, "ready");
    assert.equal(partials.length, 1);
    assert.deepEqual(partials[0]?.indexes, [0]);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient ignores stale package.json partials for other request IDs", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const stalePartial: AnalyzePackageJsonResponse = {
    version: 5,
    request_id: 198,
    sections: [],
    states: [packageJsonState("stale-lib", "loading")],
    indexes: [0],
    error: null,
    diagnostics: [],
  };
  const final: AnalyzePackageJsonResponse = {
    version: 5,
    request_id: 199,
    sections: [],
    states: [packageJsonState("react", "ready")],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(stalePartial));
      socket.write(encodeFrame(final));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const partials: AnalyzePackageJsonResponse[] = [];
    client.on("packageJsonPartial", (response: AnalyzePackageJsonResponse) => {
      partials.push(response);
    });

    const response = await client.requestAnalyzePackageJson(packageJsonRequest(199));

    assert.equal(response.request_id, 199);
    assert.equal(response.states.length, 1);
    assert.equal(partials.length, 0);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient routes registry hint refresh responses by request id", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const final: RefreshRegistryHintsResponse = {
    version: protocolVersion,
    request_id: 45,
    results: [{
      target: { name: "react", installedVersion: "18.2.0" },
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => socket.write(encodeFrame(final)), 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const response = await client.requestRefreshRegistryHints(registryRefreshRequest(45));

    assert.equal(response.results[0]?.hint?.latestVersion, "19.0.0");
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient delivers registry hint refresh partials before final response", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const partial: RefreshRegistryHintsResponse = {
    version: protocolVersion,
    request_id: 46,
    results: [{
      target: { name: "react", installedVersion: "18.2.0" },
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    indexes: [0],
    error: null,
    diagnostics: [],
  };
  const final: RefreshRegistryHintsResponse = {
    version: protocolVersion,
    request_id: 46,
    results: [{
      target: { name: "react", installedVersion: "18.2.0" },
      hint: { latestVersion: "19.0.0", isLatest: false, fetchedAt: 100 },
      error: null,
    }],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(partial));
      socket.write(encodeFrame(final));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const partials: RefreshRegistryHintsResponse[] = [];
    const response = await client.requestRefreshRegistryHints(
      registryRefreshRequest(46),
      30000,
      (item) => partials.push(item),
    );

    assert.deepEqual(partials[0]?.indexes, [0]);
    assert.equal(response.results[0]?.hint?.latestVersion, "19.0.0");
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient resolves export enumeration responses independently from batches", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const exportsResponse: EnumerateExportsResponse = {
    version: 2,
    request_id: 101,
    specifier: "tiny-lib",
    exports: ["alpha", "beta"],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(exportsResponse));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const response = await client.requestExports(exportsRequest(101));

    assert.deepEqual(response.exports, ["alpha", "beta"]);
    assert.equal(response.request_id, 101);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient resolves file size responses independently from batches", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const fileSizeResponse: FileSizeResponse = {
    version: 2,
    request_id: 102,
    raw_bytes: 100,
    minified_bytes: 80,
    gzip_bytes: 50,
    brotli_bytes: 40,
    zstd_bytes: 45,
    imports: [],
    error: null,
    diagnostics: [],
  };
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => {
      socket.write(encodeFrame(fileSizeResponse));
    }, 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const response = await client.requestFileSize(fileSizeRequest(102));

    assert.equal(response.brotli_bytes, 40);
    assert.equal(response.request_id, 102);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

test("IpcClient resolves cache management responses independently from analysis responses", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const observed: string[] = [];
  const server = net.createServer((socket) => {
    const decoder = new FrameDecoder();
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("data", (chunk) => {
      for (const message of decoder.push(chunk)) {
        const request = message as { type?: string; request_id?: number; version?: number };
        observed.push(`${request.type}:${request.request_id}`);

        if (request.type === "cache_status") {
          socket.write(encodeFrame(cacheStatusResponse(request.request_id ?? 0)));
        }

        if (request.type === "cache_cleanup") {
          socket.write(encodeFrame(cacheCleanupResponse(request.request_id ?? 0)));
        }

        if (request.type === "cache_list") {
          socket.write(encodeFrame(cacheListResponse(request.request_id ?? 0)));
        }

        if (request.type === "cache_remove") {
          socket.write(encodeFrame(cacheRemoveResponse(request.request_id ?? 0)));
        }
      }
    });
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);

    const status = await client.requestCacheStatus(cacheStatusRequest(201));
    const cleanup = await client.requestCacheCleanup(cacheCleanupRequest(202));
    const list = await client.requestCacheList(cacheListRequest(203));
    const remove = await client.requestCacheRemove(cacheRemoveRequest(204));

    assert.equal(status.project_count, 2);
    assert.equal(cleanup.total_size_bytes, 1024);
    assert.equal(list.shards.length, 1);
    assert.equal(remove.removed.length, 1);
    assert.deepEqual(observed, [
      "cache_status:201",
      "cache_cleanup:202",
      "cache_list:203",
      "cache_remove:204",
    ]);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

const workspaceReportRequest = (requestId: number): WorkspaceReportRequest => ({
  type: "workspace_report",
  version: protocolVersion,
  request_id: requestId,
  workspace_root: "C:/workspace",
  budgets: {
    perImportBrotliBytes: 1,
    perFileBrotliBytes: 1,
  },
});

test("IpcClient routes workspace report responses by request id", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const final: WorkspaceReportResponse = {
    version: protocolVersion,
    request_id: 46,
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
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.resume();
    setTimeout(() => socket.write(encodeFrame(final)), 10);
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const response = await client.requestWorkspaceReport(workspaceReportRequest(46));

    assert.equal(response.summary.importCount, 0);
    client.dispose();
  } finally {
    destroySockets(sockets);
    await closeServer(server);
  }
});

const cacheStatusResponse = (requestId: number): CacheStatusResponse => ({
  version: 6,
  request_id: requestId,
  total_size_bytes: 4096,
  project_count: 2,
  max_size_mb: 512,
  max_age_days: 30,
  last_cleanup_millis: null,
  current_project: null,
  error: null,
  diagnostics: [],
});

const cacheCleanupResponse = (requestId: number): CacheCleanupResponse => ({
  version: 6,
  request_id: requestId,
  total_size_bytes: 1024,
  removed: [],
  failed: [],
  error: null,
  diagnostics: [],
});

const cacheListResponse = (requestId: number): CacheListResponse => ({
  version: 6,
  request_id: requestId,
  shards: [
    {
      shard_id: "v1-abc",
      project_root: "/workspace",
      normalized_root: "/workspace",
      cache_path: "/cache/v1-abc/cache.redb",
      size_bytes: 1024,
      last_used_millis: null,
      loaded: true,
    },
  ],
  error: null,
  diagnostics: [],
});

const cacheRemoveResponse = (requestId: number): CacheRemoveResponse => ({
  version: 6,
  request_id: requestId,
  removed: [
    {
      shard_id: "v1-abc",
      project_root: "/workspace",
      cache_path: "/cache/v1-abc/cache.redb",
      removed: true,
      error: null,
    },
  ],
  failed: [],
  error: null,
  diagnostics: [],
});
