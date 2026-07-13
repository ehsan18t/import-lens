import assert from "node:assert/strict";
import test from "node:test";
import { bundleImpactHistoryItemForResponse } from "../../src/analysis/history.js";
import { importCostHistoryItemsForStates } from "../../src/analysis/insights.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import { isDurableFileSize, isDurableImportResult } from "../../src/analysis/transience.js";
import type { FileSizeDocumentResponse, ImportResult } from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

// The shape that has now fooled four different stores: a build that timed out (or panicked, or lost
// the engine) produces a conservative STATIC size, so the result carries `error: null`, a plausible
// byte count, and nothing at all in the fields a store normally inspects. Only the stage says so.
const degradedByTimeout: ImportResult = {
  specifier: "lodash-es",
  raw_bytes: 58,
  minified_bytes: 58,
  gzip_bytes: 58,
  brotli_bytes: 58,
  zstd_bytes: 58,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: false,
  is_cjs: false,
  confidence: "low",
  confidence_reasons: [],
  error: null,
  diagnostics: [{ stage: "timeout", message: "build exceeded BUILD_TIMEOUT", details: [] }],
};

const measured: ImportResult = {
  ...degradedByTimeout,
  raw_bytes: 17_550,
  minified_bytes: 9_000,
  gzip_bytes: 3_000,
  brotli_bytes: 2_500,
  zstd_bytes: 2_800,
  confidence: "high",
  truly_treeshakeable: true,
  diagnostics: [],
};

// A DETERMINISTIC failure is a fact about the code: it will fail the same way next time, the daemon
// caches it, and it is fine to keep.
const failedToParse: ImportResult = {
  ...measured,
  diagnostics: [{ stage: "parse", message: "unexpected token", details: [] }],
};

const stateFor = (specifier: string, result: ImportResult): ImportAnalysisState => ({
  detected: detectedImport({ specifier, packageName: specifier, named: [], importKind: "default" }),
  status: "ready",
  result,
});

const fileSize = (overrides: Partial<FileSizeDocumentResponse> = {}): FileSizeDocumentResponse => ({
  version: 7,
  request_id: 1,
  raw_bytes: 1_000,
  minified_bytes: 500,
  gzip_bytes: 200,
  brotli_bytes: 180,
  zstd_bytes: 190,
  imports: [],
  states: [],
  error: null,
  diagnostics: [],
  ...overrides,
});

test("a transiently degraded result is not durable, though nothing about it says error", () => {
  assert.equal(degradedByTimeout.error, null, "the premise: the fabrication carries no error");
  assert.equal(isDurableImportResult(degradedByTimeout), false);
  assert.equal(
    isDurableImportResult({
      ...degradedByTimeout,
      diagnostics: [{ stage: "panic", message: "unwound", details: [] }],
    }),
    false,
  );
  assert.equal(
    isDurableImportResult({
      ...degradedByTimeout,
      diagnostics: [{ stage: "engine_gone", message: "runtime dropped", details: [] }],
    }),
    false,
  );
});

test("a measurement and a deterministic failure are durable", () => {
  assert.equal(isDurableImportResult(measured), true);
  assert.equal(
    isDurableImportResult(failedToParse),
    true,
    "a parse failure is a fact about the code, not about this run of the daemon",
  );
  assert.equal(isDurableImportResult(undefined), false);
  assert.equal(isDurableImportResult({ ...measured, error: "no entry point" }), false);
});

test("the persisted import-cost history skips a transiently degraded import", () => {
  const items = importCostHistoryItemsForStates(
    [stateFor("lodash-es", degradedByTimeout), stateFor("dayjs", measured)],
    1_000,
  );

  assert.deepEqual(
    items.map((item) => item.specifier),
    ["dayjs"],
    "a fabricated 58-byte row would replace lodash-es's real baseline for good",
  );
});

test("the persisted bundle-impact history refuses a floor and takes a measurement", () => {
  assert.equal(
    bundleImpactHistoryItemForResponse(fileSize({ incomplete: true }), "C:/app/index.ts", 5),
    undefined,
    "recording a floor makes the next honest sizing of this file read as a regression",
  );
  assert.equal(
    bundleImpactHistoryItemForResponse(
      fileSize({ diagnostics: [{ stage: "panic", message: "unwound", details: [] }] }),
      "C:/app/index.ts",
      5,
    ),
    undefined,
  );
  assert.deepEqual(bundleImpactHistoryItemForResponse(fileSize(), "C:/app/index.ts", 5), {
    timestamp: 5,
    fileName: "C:/app/index.ts",
    rawBytes: 1_000,
    minifiedBytes: 500,
    gzipBytes: 200,
    brotliBytes: 180,
    zstdBytes: 190,
    importCount: 0,
  });
});

test("an incomplete or transiently degraded file total is not a durable data point", () => {
  assert.equal(isDurableFileSize(fileSize()), true);
  assert.equal(
    isDurableFileSize(fileSize({ incomplete: true })),
    false,
    "a floor is a real number, but it is not this file's size",
  );
  assert.equal(
    isDurableFileSize(
      fileSize({ diagnostics: [{ stage: "timeout", message: "combined build", details: [] }] }),
    ),
    false,
  );
  assert.equal(isDurableFileSize(fileSize({ error: "document parse failed" })), false);
  assert.equal(
    isDurableFileSize(
      fileSize({
        diagnostics: [{ stage: "entry_resolution", message: "no entry", details: [] }],
      }),
    ),
    true,
    "a deterministic diagnostic does not make the totals a floor",
  );
});
