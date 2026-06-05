import assert from "node:assert/strict";
import test from "node:test";
import { markLoadingStatesUnavailable } from "../../src/analysis/status.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { DetectedImport } from "../../src/imports/types.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

const detected: DetectedImport = {
  specifier: "react",
  packageName: "react",
  named: [],
  importKind: "default",
  syntax: "static",
  runtime: "component",
  line: 0,
  quoteEnd: { line: 0, character: 26 },
  statementRange: {
    start: { line: 0, character: 0 },
    end: { line: 0, character: 28 },
  },
};

const result: ImportResult = {
  specifier: "react",
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
};

test("markLoadingStatesUnavailable preserves completed states and marks pending states", () => {
  const missing: ImportAnalysisState = {
    detected,
    status: "missing",
    message: "Package not found",
  };
  const ready: ImportAnalysisState = {
    detected,
    status: "ready",
    result,
  };

  const states = markLoadingStatesUnavailable(
    [{ detected, status: "loading" }, missing, ready],
    "Daemon unavailable",
  );

  assert.deepEqual(states, [
    { detected, status: "unavailable", message: "Daemon unavailable" },
    missing,
    ready,
  ]);
});
