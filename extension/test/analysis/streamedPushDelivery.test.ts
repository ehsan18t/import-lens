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
 * Drives the real `IpcClient` over a real socket, exactly as `listener.analyze()` does: subscribe to
 * the push, await the response, store its states. `priorStates` is what the store already holds when
 * the response lands — none for a cold document, and the previous analysis's states for every
 * re-analysis after it.
 *
 * The two frames go out in one `write` on purpose: not a millisecond apart, not on separate ticks.
 * No amount of moving work earlier in the continuation can help, because no continuation has run.
 */
const analyzeWithSameChunkPush = async (
  generation: number,
  priorStates: ImportAnalysisState[] | null,
): Promise<ImportAnalysisState[]> => {
  const pipeName = testPipeName();
  const sockets = new Set<net.Socket>();
  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("data", () => {
      // One write, one chunk: the response and the push are dispatched on the same tick, before
      // any `await` continuation in the client can run.
      socket.write(
        Buffer.concat([
          encodeFrame(loadingResponse(generation)),
          encodeFrame(streamedPush(generation)),
        ]),
      );
    });
  });
  await listen(server, pipeName);

  try {
    const client = await IpcClient.connect(pipeName);
    const documents = new DocumentAnalysisStates();

    if (priorStates) {
      documents.set(documentKey, priorStates, generation - 1);
    }

    client.on("refreshedResults", (push: RefreshedResultsResponse) => {
      documents.applyRefreshedResults(documentKey, push.results, {
        identities: push.identities,
        isCurrent: true,
        generation: push.generation,
      });
    });

    const response = await client.requestAnalyzeDocument(analyzeRequest(generation));
    documents.set(documentKey, stateFor(response), response.request_id);
    client.dispose();

    return documents.get(documentKey);
  } finally {
    for (const socket of sockets) {
      socket.destroy();
    }
    await closeServer(server);
  }
};

/**
 * The daemon writes the analysis response and the first streamed import into the same socket, and
 * both frames routinely arrive in ONE read. `IpcClient` dispatches every frame in a chunk
 * synchronously, so the push is delivered while `listener.analyze()` is still suspended at its
 * `await` — BEFORE the continuation that stores the response's states runs. A push against a
 * document with no states used to be dropped on the floor, and that import sat at "Calculating..."
 * for ever.
 */
test("a streamed push in the SAME socket chunk as its response is not lost (cold document)", async () => {
  const states = await analyzeWithSameChunkPush(11, null);

  assert.equal(states.length, 1);
  assert.equal(
    states[0]?.status,
    "ready",
    "the push arrived before the states were stored and must still be applied",
  );
  assert.equal(states[0]?.result?.brotli_bytes, 1_500);
});

/**
 * The steady state, and the one the cold-document fix did NOT cover. On a re-analysis — every time
 * the user types — the document DOES have states, so the same-chunk push is not held: it merges into
 * the states that are on their way OUT, and the `set` storing the new response's states then writes
 * straight over it. Nothing is dropped and nothing warns; the import simply goes back to
 * "Calculating..." and stays there until the next edit, making FR-004a's "must merge each pushed
 * result into that import's state" false for the common case.
 *
 * A pushed result must therefore survive the `set` it raced.
 */
test("a streamed push in the SAME socket chunk as its response survives a RE-analysis", async () => {
  // What the previous analysis left behind: this import was still being measured then, too.
  const priorStates = stateFor(loadingResponse(11));
  const states = await analyzeWithSameChunkPush(12, priorStates);

  assert.equal(states.length, 1);
  assert.equal(
    states[0]?.status,
    "ready",
    "the push merged into the outgoing states and must not be overwritten by the new ones",
  );
  assert.equal(states[0]?.result?.brotli_bytes, 1_500);
});

/**
 * The held push must not become a second way for a SUPERSEDED result to land. A push the daemon
 * computed for an analysis the user has already typed past is dropped on arrival — and if the
 * analysis it belonged to is then abandoned (its response overtaken, so it never stores its
 * states), the push it left behind must not be replayed onto the NEXT analysis's states either.
 */
test("a held push is not replayed onto the states of a different analysis generation", () => {
  const documents = new DocumentAnalysisStates();

  documents.set(documentKey, stateFor(loadingResponse(11)), 11);
  documents.applyRefreshedResults(documentKey, [measured], {
    identities: [{ specifier: "lodash-es", import_kind: "named", named: ["debounce"] }],
    isCurrent: true,
    generation: 11,
  });
  assert.equal(documents.get(documentKey)[0]?.status, "ready", "its own generation applies it");

  // A newer analysis replaces the document's states. The generation-11 push is not part of it.
  documents.set(documentKey, stateFor(loadingResponse(12)), 12);

  assert.equal(
    documents.get(documentKey)[0]?.status,
    "loading",
    "a push from a superseded generation must not fill in a state of the current one",
  );
});

/**
 * The queue must not become a second way for a stale push to land. A push held for a document that
 * is then cleared (closed, or an analysis that failed) belongs to an abandoned analysis: it must be
 * dropped, not merged into whatever is analysed next.
 */
test("a queued push is dropped when the document is cleared before its states arrive", () => {
  const documents = new DocumentAnalysisStates();

  documents.applyRefreshedResults(documentKey, [measured], { isCurrent: true, generation: 12 });
  documents.clear(documentKey);
  documents.set(documentKey, stateFor(loadingResponse(12)), 12);

  assert.equal(
    documents.get(documentKey)[0]?.status,
    "loading",
    "a push queued before the clear belongs to the abandoned analysis",
  );
});
