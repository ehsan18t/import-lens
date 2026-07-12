import assert from "node:assert/strict";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { DocumentAnalysisStates } from "../../src/analysis/documentStates.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import { IpcClient } from "../../src/ipc/client.js";
import { encodeFrame } from "../../src/ipc/codec.js";
import type {
  AnalyzeDocumentRequest,
  AnalyzeDocumentResponse,
  ImportResult,
  RefreshedResultsResponse,
} from "../../src/ipc/protocol.js";
import { protocolVersion } from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

const documentPath = "C:/workspace/app/src/index.ts";
const documentKey = `file:///${documentPath}`;

const testPipeName = (): string => {
  const unique = `import-lens-push-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;

  if (process.platform === "win32") {
    return `\\\\.\\pipe\\${unique}`;
  }

  return path.join(tmpdir(), `${unique}.sock`);
};

const listen = async (server: net.Server, pipeName: string): Promise<void> =>
  new Promise((resolve, reject) => {
    const onError = (error: Error): void => reject(error);
    server.once("error", onError);
    server.listen(pipeName, () => {
      server.off("error", onError);
      resolve();
    });
  });

const closeServer = async (server: net.Server): Promise<void> =>
  new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });

const analyzeRequest = (requestId: number): AnalyzeDocumentRequest => ({
  type: "analyze_document",
  version: protocolVersion,
  request_id: requestId,
  workspace_root: "C:/workspace/app",
  active_document_path: documentPath,
  source: "import { debounce } from 'lodash-es';\n",
});

// What the daemon answers for a cold import: a placeholder, no result. The size arrives
// afterwards on the push channel.
const loadingResponse = (requestId: number): AnalyzeDocumentResponse => ({
  version: protocolVersion,
  request_id: requestId,
  imports: [
    {
      detected: detectedImport({
        specifier: "lodash-es",
        packageName: "lodash-es",
        named: ["debounce"],
        importKind: "named",
      }),
      status: "loading",
    },
  ],
  error: null,
  diagnostics: [],
});

const measured: ImportResult = {
  specifier: "lodash-es",
  raw_bytes: 10_000,
  minified_bytes: 4_000,
  gzip_bytes: 1_800,
  brotli_bytes: 1_500,
  zstd_bytes: 1_700,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: [],
  error: null,
  diagnostics: [],
};

const streamedPush = (generation: number): RefreshedResultsResponse => ({
  type: "refreshed_results",
  version: protocolVersion,
  workspace_root: "C:/workspace/app",
  document_path: documentPath,
  results: [measured],
  identities: [{ specifier: "lodash-es", import_kind: "named", named: ["debounce"] }],
  generation,
});

const stateFor = (response: AnalyzeDocumentResponse): ImportAnalysisState[] =>
  response.imports.map((item) => ({
    detected: item.detected,
    status: item.status,
    result: item.result,
    message: item.message,
  }));

/**
 * The daemon writes the analysis response and the first streamed import into the same socket, and
 * both frames routinely arrive in ONE read. `IpcClient` dispatches every frame in a chunk
 * synchronously, so the push is delivered while `listener.analyze()` is still suspended at its
 * `await` — BEFORE the continuation that stores the response's states runs. A push against a
 * document with no states used to be dropped on the floor, and that import sat at "Calculating..."
 * for ever.
 *
 * The frames go out in one `write` on purpose: not a millisecond apart, not on separate ticks. No
 * amount of moving work earlier in the continuation can help, because no continuation has run.
 */
test("a streamed push in the SAME socket chunk as its response is not lost", async () => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("data", () => {
      socket.write(
        Buffer.concat([encodeFrame(loadingResponse(11)), encodeFrame(streamedPush(11))]),
      );
    });
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const documents = new DocumentAnalysisStates();
    client.on("refreshedResults", (push: RefreshedResultsResponse) => {
      documents.applyRefreshedResults(documentKey, push.results, {
        identities: push.identities,
        isCurrent: true,
      });
    });

    // Exactly the shape of `listener.analyze()`: await the response, then store its states.
    const response = await client.requestAnalyzeDocument(analyzeRequest(11));
    documents.set(documentKey, stateFor(response));

    const states = documents.get(documentKey);
    assert.equal(states.length, 1);
    assert.equal(
      states[0]?.status,
      "ready",
      "the push arrived before the states were stored and must still be applied",
    );
    assert.equal(states[0]?.result?.brotli_bytes, 1_500);
    client.dispose();
  } finally {
    for (const socket of sockets) {
      socket.destroy();
    }
    await closeServer(server);
  }
});

/**
 * The queue must not become a second way for a stale push to land. A push held for a document that
 * is then cleared (closed, or an analysis that failed) belongs to an abandoned analysis: it must be
 * dropped, not merged into whatever is analysed next.
 */
test("a queued push is dropped when the document is cleared before its states arrive", () => {
  const documents = new DocumentAnalysisStates();

  documents.applyRefreshedResults(documentKey, [measured], { isCurrent: true });
  documents.clear(documentKey);
  documents.set(documentKey, stateFor(loadingResponse(12)));

  assert.equal(
    documents.get(documentKey)[0]?.status,
    "loading",
    "a push queued before the clear belongs to the abandoned analysis",
  );
});
