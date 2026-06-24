import assert from "node:assert/strict";
import test from "node:test";
import {
  applyFinalBatchResults,
  markLoadingStatesUnavailable,
} from "../../src/analysis/status.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { DetectedImport } from "../../src/ipc/protocol.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const detected: DetectedImport = detectedImport({
  specifier: "react",
  packageName: "react",
  importKind: "default",
  quoteEnd: { line: 0, character: 26 },
  specifierRange: sourceRange(0, 8, 25),
  statementRange: sourceRange(0, 0, 28),
});

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

test("applyFinalBatchResults marks missing daemon results unavailable", () => {
  const warnings: string[] = [];

  const states = applyFinalBatchResults(
    [
      { detected, status: "loading" },
      {
        detected: detectedImport({
          specifier: "lodash-es",
          packageName: "lodash-es",
          importKind: "namespace",
          quoteEnd: { line: 1, character: 28 },
          specifierRange: sourceRange(1, 8, 27),
          statementRange: sourceRange(1, 0, 30),
        }),
        status: "loading",
      },
    ],
    [result],
    (specifier, reason) => warnings.push(`${specifier}: ${reason}`),
  );

  assert.equal(states[0]?.status, "ready");
  assert.deepEqual(states[1], {
    detected: states[1]?.detected,
    status: "unavailable",
    message: "No daemon response",
  });
  assert.deepEqual(warnings, [
    "lodash-es: daemon response did not include a matching result",
  ]);
});

test("applyFinalBatchResults keeps partial ready states when final response is incomplete", () => {
  const states = applyFinalBatchResults(
    [{ detected, status: "ready", result }],
    [],
    () => undefined,
  );

  assert.deepEqual(states, [{ detected, status: "ready", result }]);
});
