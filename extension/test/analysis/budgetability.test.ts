import assert from "node:assert/strict";
import test from "node:test";
import {
  isBudgetableFileSize,
  isBudgetableImportResult,
} from "../../src/analysis/budgetability.js";
import { isDurableFileSize, isDurableImportResult } from "../../src/analysis/transience.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

const impreciseDiagnostic = {
  stage: "imprecise_assets",
  message: "stylesheets were measured separately, so this size may read high",
  details: [],
};

const impreciseImport: ImportResult = {
  specifier: "asset-lib",
  raw_bytes: 3000,
  minified_bytes: 2800,
  gzip_bytes: 2600,
  brotli_bytes: 2500,
  zstd_bytes: 2550,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "medium",
  confidence_reasons: ["Stylesheets were measured separately."],
  error: null,
  diagnostics: [impreciseDiagnostic],
};

test("an imprecise asset upper bound stays durable but is never budgetable", () => {
  assert.equal(isDurableImportResult(impreciseImport), true);
  assert.equal(isBudgetableImportResult(impreciseImport), false);

  const fileSize = {
    error: null,
    incomplete: false,
    degraded: false,
    diagnostics: [impreciseDiagnostic],
  };
  assert.equal(isDurableFileSize(fileSize), true);
  assert.equal(isBudgetableFileSize(fileSize), false);
});
