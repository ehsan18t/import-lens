import assert from "node:assert/strict";
import test from "node:test";
import type { DetectedImport } from "../../src/imports/types.js";
import type { BatchRequest, BatchResponse, ImportRequest, ImportResult } from "../../src/ipc/protocol.js";
import {
  analyzeScannedImports,
  chunkArray,
  sortWorkspaceUris,
  workspaceExcludePattern,
  workspaceIncludePattern,
  type ScannedImport,
} from "../../src/report/workspaceScanner.js";

test("workspace scanner uses supported source include and generated-folder exclude patterns", () => {
  assert.equal(workspaceIncludePattern, "**/*.{js,jsx,ts,tsx,svelte,astro}");
  assert.equal(workspaceExcludePattern, "**/{node_modules,dist,build,out,coverage}/**");
});

test("sortWorkspaceUris orders files by fsPath for deterministic report batches", () => {
  const sorted = sortWorkspaceUris([
    { fsPath: "/workspace/src/z.ts" },
    { fsPath: "/workspace/src/a.ts" },
  ]);

  assert.deepEqual(sorted.map((uri) => uri.fsPath), ["/workspace/src/a.ts", "/workspace/src/z.ts"]);
});

test("chunkArray keeps daemon report batches under the configured size", () => {
  assert.deepEqual(chunkArray([1, 2, 3, 4, 5], 2), [[1, 2], [3, 4], [5]]);
});

test("analyzeScannedImports sends daemon batches per source file", async () => {
  const batches: BatchRequest[] = [];
  const daemon = {
    state: "ready" as const,
    sendBatch: async (request: BatchRequest): Promise<BatchResponse> => {
      batches.push(request);
      return {
        version: request.version,
        request_id: request.request_id,
        imports: request.imports.map((item) => result(item.specifier)),
      };
    },
  };

  const items = await analyzeScannedImports(
    [
      scanned("react", "/workspace/src/a.ts"),
      scanned("lodash-es", "/workspace/src/b.ts"),
    ],
    daemon,
    { chunkSize: 10, nextRequestId: requestIdGenerator() },
  );

  assert.deepEqual(batches.map((batch) => batch.active_document_path), [
    "/workspace/src/a.ts",
    "/workspace/src/b.ts",
  ]);
  assert.deepEqual(items.map((item) => item.result?.specifier), ["react", "lodash-es"]);
});

const requestIdGenerator = (): (() => number) => {
  let requestId = 0;
  return () => {
    requestId += 1;
    return requestId;
  };
};

const detected = (specifier: string): DetectedImport => ({
  specifier,
  packageName: specifier,
  named: [],
  importKind: "namespace",
  runtime: "component",
  line: 0,
  quoteEnd: { line: 0, character: 20 },
  statementRange: {
    start: { line: 0, character: 0 },
    end: { line: 0, character: 21 },
  },
});

const request = (specifier: string): ImportRequest => ({
  specifier,
  package: specifier,
  version: "1.0.0",
  named: [],
  import_kind: "namespace",
  runtime: "component",
});

const scanned = (specifier: string, sourceFile: string): ScannedImport => ({
  detected: detected(specifier),
  sourceFile,
  workspaceRoot: "/workspace",
  request: request(specifier),
});

const result = (specifier: string): ImportResult => ({
  specifier,
  raw_bytes: 10,
  minified_bytes: 8,
  gzip_bytes: 7,
  brotli_bytes: 6,
  zstd_bytes: 5,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  error: null,
  diagnostics: [],
});
