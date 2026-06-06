import assert from "node:assert/strict";
import test from "node:test";
import type { ImportResult } from "../../src/ipc/protocol.js";
import {
  buildDuplicateImportGroups,
  buildDuplicateModuleGroups,
  buildReportRows,
  buildReportSummary,
  type WorkspaceReportItem,
} from "../../src/report/reportModel.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const detected = (specifier: string) => detectedImport({
  specifier,
  packageName: specifier,
  line: 2,
  quoteEnd: { line: 2, character: 20 },
  specifierRange: sourceRange(2, 8, 18),
  statementRange: sourceRange(2, 0, 21),
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
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
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

test("buildDuplicateImportGroups aggregates repeated specifiers across files", () => {
  const rows = buildReportRows([
    item("react", 100),
    {
      ...item("react", 80),
      sourceFile: "/workspace/src/other.ts",
    },
    item("zod", 50),
  ]);

  assert.deepEqual(buildDuplicateImportGroups(rows), [
    {
      specifier: "react",
      count: 2,
      totalBrotliBytes: 180,
      sourceFiles: ["src/other.ts", "src/react.ts"],
    },
  ]);
});

test("buildDuplicateModuleGroups finds shared module paths across report rows", () => {
  const sharedPath = "/workspace/node_modules/shared/dist/index.js";
  const rows = buildReportRows([
    {
      ...item("left", 100),
      result: {
        ...result("left", 100),
        module_breakdown: [{ path: sharedPath, bytes: 60 }],
      },
    },
    {
      ...item("right", 90),
      result: {
        ...result("right", 90),
        module_breakdown: [{ path: sharedPath, bytes: 60 }],
      },
    },
  ]);

  assert.deepEqual(buildDuplicateModuleGroups(rows), [
    {
      modulePath: sharedPath,
      basename: "index.js",
      count: 2,
      totalBytes: 120,
      specifiers: ["left", "right"],
      vendored: false,
    },
  ]);
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

test("buildReportRows explains shared dependency bytes", () => {
  const rows = buildReportRows([
    {
      ...item("tiny-lib", 100),
      result: {
        ...result("tiny-lib", 100),
        shared_bytes: 25,
      },
    },
  ]);

  assert.match(rows[0].warning, /Shares 25 B with other imports/u);
});

test("buildReportRows carries confidence and confidence reasons", () => {
  const rows = buildReportRows([
    {
      ...item("fallback-lib", 100),
      result: {
        ...result("fallback-lib", 100),
        confidence: "low",
        confidence_reasons: ["Static entry sizing is a fallback."],
      },
    },
  ]);

  assert.equal(rows[0].confidence, "low");
  assert.equal(rows[0].confidenceReasons, "Static entry sizing is a fallback.");
  assert.match(rows[0].warning, /Low confidence/u);
});

test("buildReportRows reports configured per-import budget violations", () => {
  const rows = buildReportRows([item("large", 2500)], { perImportBrotliBytes: 2000 });
  const summary = buildReportSummary(rows);

  assert.match(rows[0].warning, /Budget exceeded/u);
  assert.equal(summary.budgetViolationCount, 1);
});

test("buildReportSummary totals imports and builds largest-contributor treemap data", () => {
  const rows = buildReportRows([
    item("small", 10),
    {
      ...item("large", 90),
      result: {
        ...result("large", 90),
        confidence: "medium",
      },
    },
  ]);
  const summary = buildReportSummary(rows);

  assert.equal(summary.importCount, 2);
  assert.equal(summary.totalBrotliBytes, 100);
  assert.equal(summary.budgetViolationCount, 0);
  assert.deepEqual(summary.duplicateImports, []);
  assert.deepEqual(summary.sharedModules, []);
  assert.deepEqual(
    summary.treemap.map((item) => [item.specifier, item.brotliBytes, item.percentage, item.confidence]),
    [
      ["large", 90, 90, "medium"],
      ["small", 10, 10, "high"],
    ],
  );
});
