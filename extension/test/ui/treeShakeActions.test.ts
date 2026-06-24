import assert from "node:assert/strict";
import test from "node:test";
import { shouldOfferNamedExportCandidates } from "../../src/ui/namedExportCandidatePolicy.js";
import { treeShakeActionReason } from "../../src/ui/treeShakeActionReason.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { DetectedImport } from "../../src/ipc/protocol.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "tiny-lib",
  raw_bytes: 100,
  minified_bytes: 80,
  gzip_bytes: 70,
  brotli_bytes: 60,
  zstd_bytes: 65,
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

test("treeShakeActionReason explains non tree-shakeable import results", () => {
  assert.match(treeShakeActionReason(result({ is_cjs: true })) ?? "", /CommonJS/u);
  assert.match(treeShakeActionReason(result({ side_effects: true })) ?? "", /side effects/u);
  assert.match(treeShakeActionReason(result({ truly_treeshakeable: false })) ?? "", /not tree-shakeable/u);
});

test("treeShakeActionReason ignores already tree-shakeable and errored imports", () => {
  assert.equal(treeShakeActionReason(result()), null);
  assert.equal(treeShakeActionReason(result({ error: "failed" })), null);
});

const detected = (overrides: Partial<DetectedImport> = {}): DetectedImport => detectedImport({
  specifier: "date-fns",
  packageName: "date-fns",
  quoteEnd: { line: 0, character: 31 },
  specifierRange: sourceRange(0, 8, 30),
  statementRange: sourceRange(0, 0, 33),
  ...overrides,
});

const state = (
  detectedOverrides: Partial<DetectedImport> = {},
  resultOverrides: Partial<ImportResult> = {},
): ImportAnalysisState => ({
  detected: detected(detectedOverrides),
  status: "ready",
  result: result({ truly_treeshakeable: false, ...resultOverrides }),
});

test("shouldOfferNamedExportCandidates targets namespace imports that do not tree-shake", () => {
  assert.equal(shouldOfferNamedExportCandidates(state()), true);
  assert.equal(shouldOfferNamedExportCandidates(state({ importKind: "named", named: ["format"] })), false);
  assert.equal(shouldOfferNamedExportCandidates(state({}, { truly_treeshakeable: true })), false);
  assert.equal(shouldOfferNamedExportCandidates(state({}, { error: "failed" })), false);
});
