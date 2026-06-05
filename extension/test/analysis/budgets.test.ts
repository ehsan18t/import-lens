import assert from "node:assert/strict";
import test from "node:test";
import {
  budgetInsightForState,
  budgetViolationsForStates,
  sanitizeBudgets,
} from "../../src/analysis/budgets.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { DetectedImport } from "../../src/imports/types.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

const detected = (specifier: string, line: number): DetectedImport => ({
  specifier,
  packageName: specifier,
  named: [],
  importKind: "namespace",
  syntax: "static",
  runtime: "component",
  line,
  quoteEnd: { line, character: 20 },
  statementRange: {
    start: { line, character: 0 },
    end: { line, character: 24 },
  },
});

const result = (specifier: string, brotliBytes: number): ImportResult => ({
  specifier,
  raw_bytes: brotliBytes + 100,
  minified_bytes: brotliBytes + 50,
  gzip_bytes: brotliBytes + 20,
  brotli_bytes: brotliBytes,
  zstd_bytes: brotliBytes + 10,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
});

const ready = (specifier: string, brotliBytes: number, line = 0): ImportAnalysisState => ({
  detected: detected(specifier, line),
  status: "ready",
  result: result(specifier, brotliBytes),
});

test("sanitizeBudgets accepts positive thresholds and drops invalid values", () => {
  assert.deepEqual(
    sanitizeBudgets({
      perImportBrotliBytes: 1500,
      perFileBrotliBytes: 4000,
      ignored: true,
    }),
    {
      perImportBrotliBytes: 1500,
      perFileBrotliBytes: 4000,
    },
  );
  assert.deepEqual(
    sanitizeBudgets({
      perImportBrotliBytes: -1,
      perFileBrotliBytes: Number.NaN,
    }),
    {},
  );
});

test("budgetViolationsForStates reports per-import and per-file brotli budget violations", () => {
  const states = [ready("large-lib", 2600, 2), ready("small-lib", 900, 3)];

  assert.deepEqual(
    budgetViolationsForStates(states, {
      perImportBrotliBytes: 2000,
      perFileBrotliBytes: 3000,
    }).map((violation) => ({
      kind: violation.kind,
      specifier: violation.specifier,
      actualBytes: violation.actualBytes,
      limitBytes: violation.limitBytes,
      line: violation.range.start.line,
    })),
    [
      { kind: "import", specifier: "large-lib", actualBytes: 2600, limitBytes: 2000, line: 2 },
      { kind: "file", specifier: undefined, actualBytes: 3500, limitBytes: 3000, line: 2 },
    ],
  );
});

test("budgetInsightForState adds distinct inline and hover text for import budget violations", () => {
  assert.deepEqual(
    budgetInsightForState(ready("large-lib", 2600), { perImportBrotliBytes: 2000 }),
    {
      label: "over budget",
      tooltip: "Budget: large-lib is 2.6 kB br, over the per-import budget of 2.0 kB br.",
    },
  );
  assert.equal(budgetInsightForState(ready("small-lib", 900), { perImportBrotliBytes: 2000 }), null);
});
