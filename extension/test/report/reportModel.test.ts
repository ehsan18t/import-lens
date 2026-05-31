import assert from "node:assert/strict";
import test from "node:test";
import type { DetectedImport } from "../../src/imports/types.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { buildReportRows, type WorkspaceReportItem } from "../../src/report/reportModel.js";

const detected = (specifier: string): DetectedImport => ({
  specifier,
  packageName: specifier,
  named: [],
  importKind: "namespace",
  runtime: "component",
  line: 2,
  quoteEnd: { line: 2, character: 20 },
  statementRange: {
    start: { line: 2, character: 0 },
    end: { line: 2, character: 21 },
  },
});

const result = (specifier: string, brotliBytes: number): ImportResult => ({
  specifier,
  raw_bytes: brotliBytes + 20,
  minified_bytes: brotliBytes + 10,
  gzip_bytes: brotliBytes + 5,
  brotli_bytes: brotliBytes,
  zstd_bytes: brotliBytes + 2,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  error: null,
  diagnostics: [],
});

const item = (specifier: string, brotliBytes: number): WorkspaceReportItem => ({
  detected: detected(specifier),
  sourceFile: `/workspace/src/${specifier}.ts`,
  workspaceRoot: "/workspace",
  result: result(specifier, brotliBytes),
});

test("buildReportRows sorts imports by brotli size descending", () => {
  const rows = buildReportRows([item("small", 10), item("large", 100)]);

  assert.deepEqual(rows.map((row) => row.specifier), ["large", "small"]);
});

test("buildReportRows includes warning rows without size results", () => {
  const rows = buildReportRows([
    {
      detected: detected("missing"),
      sourceFile: "/workspace/src/app.ts",
      workspaceRoot: "/workspace",
      warning: "Package not found",
    },
  ]);

  assert.equal(rows[0].warning, "Package not found");
  assert.equal(rows[0].brotliBytes, 0);
});

test("buildReportRows summarizes the largest module contributors", () => {
  const rows = buildReportRows([
    {
      ...item("tiny-lib", 100),
      result: {
        ...result("tiny-lib", 100),
        module_breakdown: [
          { path: "/workspace/node_modules/tiny-lib/large.js", bytes: 1200 },
          { path: "/workspace/node_modules/tiny-lib/small.js", bytes: 80 },
        ],
      },
    },
  ]);

  assert.equal(rows[0].topModules, "large.js (1200 B), small.js (80 B)");
});

test("buildReportRows carries shared file bytes", () => {
  const rows = buildReportRows([
    {
      ...item("tiny-lib", 100),
      result: {
        ...result("tiny-lib", 100),
        shared_bytes: 25,
      },
    },
  ]);

  assert.equal(rows[0].sharedBytes, 25);
});
