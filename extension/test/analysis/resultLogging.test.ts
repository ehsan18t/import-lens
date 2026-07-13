import assert from "node:assert/strict";
import test from "node:test";
import {
  ImportResultLogTracker,
  warningMessageForImportResult,
} from "../../src/analysis/resultLogging.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

const result = (overrides: Partial<ImportResult> = {}): ImportResult => ({
  specifier: "sonner",
  raw_bytes: 1200,
  minified_bytes: 900,
  gzip_bytes: 600,
  brotli_bytes: 480,
  zstd_bytes: 520,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["OXC completed without precision warnings."],
  error: null,
  diagnostics: [],
  ...overrides,
});

test("warningMessageForImportResult stays quiet for usable low-confidence fallbacks", () => {
  assert.equal(
    warningMessageForImportResult(
      result({
        confidence: "low",
        confidence_reasons: ["Static entry sizing is a fallback."],
        diagnostics: [
          {
            stage: "oxc_fallback",
            message: "OXC pipeline failed; using static entry sizing.",
            details: ["failed_stage: minify"],
          },
        ],
      }),
    ),
    null,
  );
});

test("warningMessageForImportResult stays quiet for measured results that carry fallback error details", () => {
  assert.equal(
    warningMessageForImportResult(
      result({
        confidence: "low",
        error: "failed to minify bundled modules; using static entry sizing",
      }),
    ),
    null,
  );
});

test("warningMessageForImportResult warns when no usable size was produced", () => {
  assert.equal(
    warningMessageForImportResult(
      result({
        raw_bytes: null,
        minified_bytes: null,
        gzip_bytes: null,
        brotli_bytes: null,
        zstd_bytes: null,
        confidence: "low",
        error: "failed to resolve package entry",
        unmeasured_stage: "entry_resolution",
      }),
    ),
    "sonner: failed to resolve package entry",
  );
});

test("ImportResultLogTracker deduplicates partial and final warning/debug logs", () => {
  const warnings: string[] = [];
  const debug: string[] = [];
  const logger = {
    warn: (message: string) => warnings.push(message),
    debug: (message: string) => debug.push(message),
  };
  const tracker = new ImportResultLogTracker(logger, 7);
  const failed = result({
    raw_bytes: null,
    minified_bytes: null,
    gzip_bytes: null,
    brotli_bytes: null,
    zstd_bytes: null,
    confidence: "low",
    error: "failed to resolve package entry",
    unmeasured_stage: "entry_resolution",
    diagnostics: [
      {
        stage: "entry_resolution",
        message: "failed to resolve package entry",
        details: ["specifier: sonner"],
      },
    ],
  });

  tracker.logResult(failed);
  tracker.logResult(failed);
  tracker.logMissingResult("sonner", "daemon response did not include a matching result");
  tracker.logMissingResult("sonner", "daemon response did not include a matching result");

  assert.deepEqual(warnings, [
    "sonner: failed to resolve package entry",
    "sonner: daemon response did not include a matching result",
  ]);
  assert.equal(debug.length, 1);
  assert.match(debug[0] ?? "", /Import Lens diagnostics for sonner/u);
});
