import assert from "node:assert/strict";
import test from "node:test";
import { applyImportAnalysisInsights } from "../../src/analysis/insights.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { ImportLensConfig } from "../../src/config.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { importHintParts } from "../../src/ui/importHintParts.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const config = (overrides: Partial<ImportLensConfig> = {}): ImportLensConfig => ({
  enabled: true,
  display: "inlayHint",
  inlineRenderer: "colored",
  compression: "brotli",
  debounceMs: 300,
  showWarnings: true,
  useCodeLens: false,
  enableDiskCache: true,
  cacheMaxSizeMB: 512,
  cacheMaxAgeDays: 30,
  enableRegistryHints: false,
  logLevel: "error",
  budgets: { perImportBrotliBytes: 1000 },
  ...overrides,
});

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "lodash-es",
  raw_bytes: 18000,
  minified_bytes: 5300,
  gzip_bytes: 1800,
  brotli_bytes: 1500,
  zstd_bytes: 1600,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
  ...overrides,
});

const readyState = (overrides: Partial<ImportAnalysisState> = {}): ImportAnalysisState => ({
  detected: detectedImport({
    specifier: "lodash-es",
    specifierRange: sourceRange(0, 8, 20),
    statementRange: sourceRange(0, 0, 24),
  }),
  status: "ready",
  result: result(),
  ...overrides,
});

test("importHintParts splits size, tags, and insight suffixes", () => {
  const [withInsights] = applyImportAnalysisInsights(
    [readyState({ result: result({ is_cjs: true, confidence: "low" }) })],
    { changedLines: new Set([0]), importCostHistory: [] },
  );

  const parts = importHintParts(withInsights, config());

  assert.deepEqual(parts, {
    primary: "~1.5 kB br",
    primaryTone: "sizeLow",
    suffixes: [
      { text: "CJS", tone: "tag" },
      { text: "+1.5 kB br", tone: "delta" },
    ],
  });
});

test("importHintParts returns neutral loading and missing states", () => {
  assert.deepEqual(importHintParts({ ...readyState(), status: "loading" }, config()), {
    primary: "Calculating...",
    primaryTone: "neutral",
    suffixes: [],
  });
  assert.deepEqual(
    importHintParts({ ...readyState(), status: "missing", message: "Missing pkg" }, config()),
    {
      primary: "Missing pkg",
      primaryTone: "neutral",
      suffixes: [],
    },
  );
  assert.equal(importHintParts({ ...readyState(), status: "unavailable" }, config()), null);
});

test("importHintParts maps budget insight to alert tone", () => {
  const [state] = applyImportAnalysisInsights([readyState()], {
    importCostHistory: [],
    budgets: { perImportBrotliBytes: 1000 },
  });

  const parts = importHintParts(state, config({ budgets: { perImportBrotliBytes: 1000 } }));

  assert.deepEqual(parts?.suffixes.at(-1), { text: "over budget", tone: "alert" });
});
