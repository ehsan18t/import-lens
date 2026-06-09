import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import type { BatchRequest, BatchResponse, ImportRequest, ImportResult } from "../../src/ipc/protocol.js";
import {
  analyzeScannedImports,
  chunkArray,
  scanWorkspaceImports,
  sortWorkspaceUris,
  workspaceExcludePattern,
  workspaceIncludePattern,
  type ScannedImport,
  type WorkspaceScannerApi,
  type WorkspaceTextDocument,
  type WorkspaceUri,
} from "../../src/report/workspaceScanner.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

test("workspace scanner uses supported source include and generated-folder exclude patterns", () => {
  assert.equal(workspaceIncludePattern, "**/*.{js,jsx,ts,tsx,mts,cts,svelte,astro,vue}");
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

test("scanWorkspaceImports opens files concurrently with deterministic output order", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "import-lens-scan-"));
  const files = [
    path.join(root, "src", "c.ts"),
    path.join(root, "src", "a.ts"),
    path.join(root, "src", "b.ts"),
  ];
  const sources = new Map([
    [files[0], "import c from 'missing-scan-c';"],
    [files[1], "import a from 'missing-scan-a';"],
    [files[2], "import b from 'missing-scan-b';"],
  ]);
  let activeOpenCount = 0;
  let maxActiveOpenCount = 0;

  try {
    const workspace = fakeWorkspace({
      root,
      files,
      openTextDocument: async (uri) => {
        activeOpenCount += 1;
        maxActiveOpenCount = Math.max(maxActiveOpenCount, activeOpenCount);

        try {
          await delay(20);
          return fakeDocument(uri, sources.get(uri.fsPath) ?? "");
        } finally {
          activeOpenCount -= 1;
        }
      },
    });

    const scanned = await scanWorkspaceImports(workspace, { scanConcurrency: 2 });

    assert.equal(maxActiveOpenCount, 2);
    assert.deepEqual(scanned.map((item) => item.sourceFile), [
      files[1],
      files[2],
      files[0],
    ]);
    assert.deepEqual(scanned.map((item) => item.warning), [
      "Package not found",
      "Package not found",
      "Package not found",
    ]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("scanWorkspaceImports skips unreadable files instead of aborting the workspace scan", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "import-lens-scan-"));
  const unreadableFile = path.join(root, "src", "broken.ts");
  const readableFile = path.join(root, "src", "ready.ts");

  try {
    const workspace = fakeWorkspace({
      root,
      files: [unreadableFile, readableFile],
      openTextDocument: async (uri) => {
        if (uri.fsPath === unreadableFile) {
          throw new Error("Cannot open test document");
        }

        return fakeDocument(uri, "import ready from 'missing-readable';");
      },
    });

    const scanned = await scanWorkspaceImports(workspace, { scanConcurrency: 2 });

    assert.equal(scanned.length, 1);
    assert.equal(scanned[0]?.sourceFile, readableFile);
    assert.equal(scanned[0]?.warning, "Package not found");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
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

test("analyzeScannedImports default request IDs stay unique across report scans", async () => {
  const originalNow = Date.now;
  const requestIds: number[] = [];
  Date.now = () => 1_700_000;
  const daemon = {
    state: "ready" as const,
    sendBatch: async (request: BatchRequest): Promise<BatchResponse> => {
      requestIds.push(request.request_id);
      return {
        version: request.version,
        request_id: request.request_id,
        imports: request.imports.map((item) => result(item.specifier)),
      };
    },
  };

  try {
    await Promise.all([
      analyzeScannedImports([scanned("react", "/workspace/src/a.ts")], daemon),
      analyzeScannedImports([scanned("lodash-es", "/workspace/src/b.ts")], daemon),
    ]);
  } finally {
    Date.now = originalNow;
  }

  assert.equal(new Set(requestIds).size, requestIds.length);
});

const requestIdGenerator = (): (() => number) => {
  let requestId = 0;
  return () => {
    requestId += 1;
    return requestId;
  };
};

const delay = (milliseconds: number): Promise<void> =>
  new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });

const fakeDocument = (uri: WorkspaceUri, source: string): WorkspaceTextDocument => ({
  uri,
  fileName: uri.fsPath,
  getText: () => source,
});

const fakeWorkspace = ({
  root,
  files,
  openTextDocument,
}: {
  root: string;
  files: readonly string[];
  openTextDocument: WorkspaceScannerApi["openTextDocument"];
}): WorkspaceScannerApi => ({
  findFiles: async () => files.map((fsPath) => ({ fsPath })),
  openTextDocument,
  getWorkspaceFolder: () => ({ uri: { fsPath: root } }),
});

const detected = (specifier: string) => detectedImport({
  specifier,
  packageName: specifier,
  quoteEnd: { line: 0, character: 20 },
  specifierRange: sourceRange(0, 8, 18),
  statementRange: sourceRange(0, 0, 21),
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
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
});
