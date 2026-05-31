import assert from "node:assert/strict";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { IpcClient } from "../../src/ipc/client.js";
import { encodeFrame } from "../../src/ipc/codec.js";
import type { BatchRequest, BatchResponse, ImportResult } from "../../src/ipc/protocol.js";

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
