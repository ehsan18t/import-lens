import assert from "node:assert/strict";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { IpcClient } from "../../src/ipc/client.js";
import { encodeFrame } from "../../src/ipc/codec.js";
import type {
  AnalyzePackageJsonRequest,
  AnalyzePackageJsonResponse,
  BatchRequest,
  BatchResponse,
  EnumerateExportsRequest,
  EnumerateExportsResponse,
  FileSizeRequest,
  FileSizeResponse,
  ImportResult,
  PackageJsonDependencyAnalysisItem,
  PackageJsonDependencyEntry,
} from "../../src/ipc/protocol.js";

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
