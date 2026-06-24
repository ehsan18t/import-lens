import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import type { AnalyzeDocumentRequest, AnalyzeDocumentResponse, ImportResult } from "../../src/ipc/protocol.js";
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

    const scanned = await scanWorkspaceImports(workspace, fakeDaemon(), { scanConcurrency: 2 });

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

    const scanned = await scanWorkspaceImports(workspace, fakeDaemon(), { scanConcurrency: 2 });

    assert.equal(scanned.length, 1);
    assert.equal(scanned[0]?.sourceFile, readableFile);
    assert.equal(scanned[0]?.warning, "Package not found");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("analyzeScannedImports maps daemon document states into report items", async () => {
  const items = await analyzeScannedImports([
    scanned("react", "/workspace/src/a.ts", result("react")),
    scanned("lodash-es", "/workspace/src/b.ts", result("lodash-es")),
  ]);

  assert.deepEqual(items.map((item) => item.result?.specifier), ["react", "lodash-es"]);
});

test("scanWorkspaceImports uses unique daemon request IDs", async () => {
  const requestIds: number[] = [];
  const daemon = {
    state: "ready" as const,
    analyzeDocument: async (request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse> => {
      requestIds.push(request.request_id);
      return {
        version: request.version,
        request_id: request.request_id,
        imports: [],
        error: null,
        diagnostics: [],
      };
    },
  };
  const workspace = fakeWorkspace({
    root: "/workspace",
    files: ["/workspace/src/a.ts", "/workspace/src/b.ts"],
    openTextDocument: async (uri) => fakeDocument(uri, "import value from 'react';"),
  });

  await scanWorkspaceImports(workspace, daemon);

  assert.equal(new Set(requestIds).size, requestIds.length);
});

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

const fakeDaemon = () => ({
  state: "ready" as const,
  analyzeDocument: async (request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse> => {
    const specifier = request.source.match(/['"]([^'"]+)['"]/)?.[1] ?? "missing";

    return {
      version: request.version,
      request_id: request.request_id,
      imports: [
        {
          detected: detected(specifier),
          status: "missing",
          message: "Package not found",
        },
      ],
      error: null,
      diagnostics: [],
    };
  },
});

const detected = (specifier: string) => detectedImport({
  specifier,
  packageName: specifier,
  quoteEnd: { line: 0, character: 20 },
  specifierRange: sourceRange(0, 8, 18),
  statementRange: sourceRange(0, 0, 21),
});

const scanned = (specifier: string, sourceFile: string, importResult?: ImportResult): ScannedImport => ({
  detected: detected(specifier),
  sourceFile,
  workspaceRoot: "/workspace",
  result: importResult,
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
