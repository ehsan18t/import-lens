import assert from "node:assert/strict";
import test from "node:test";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { formatImportSize } from "../../src/ui/format.js";

const result: ImportResult = {
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
};

test("formatImportSize supports minimal, standard, and verbose display modes", () => {
  assert.equal(formatImportSize(result, { display: "minimal", compression: "brotli", showWarnings: true }), "1.5 kB br");
  assert.equal(formatImportSize(result, { display: "standard", compression: "brotli", showWarnings: true }), "1.5 kB br · 5.3 kB min");
  assert.equal(formatImportSize(result, { display: "verbose", compression: "brotli", showWarnings: true }), "1.5 kB br · 1.8 kB gz · 1.6 kB zstd · 5.3 kB min");
});

test("formatImportSize shows unavailable and warning states", () => {
  assert.equal(formatImportSize({ ...result, error: "parse failed" }, { display: "standard", compression: "brotli", showWarnings: true }), "Size unavailable");
  assert.equal(formatImportSize({ ...result, side_effects: true }, { display: "minimal", compression: "gzip", showWarnings: true }), "1.8 kB gz");
  assert.equal(formatImportSize({ ...result, truly_treeshakeable: false }, { display: "minimal", compression: "brotli", showWarnings: true }), "1.5 kB br");
  assert.equal(formatImportSize({ ...result, is_cjs: true, confidence: "low" }, { display: "minimal", compression: "zstd", showWarnings: true }), "~1.6 kB zstd · CJS");
  assert.equal(formatImportSize({ ...result, confidence: "low" }, { display: "standard", compression: "brotli", showWarnings: true }), "~1.5 kB br · 5.3 kB min");
});

test("formatImportSize can hide tree-shaking warning indicators", () => {
  assert.equal(
    formatImportSize(
      { ...result, is_cjs: true, side_effects: true, truly_treeshakeable: false },
      { display: "minimal", compression: "brotli", showWarnings: false },
    ),
    "1.5 kB br",
  );
});

test("formatImportSize labels server-side import runtime", () => {
  assert.equal(
    formatImportSize(result, { display: "minimal", compression: "brotli", showWarnings: true }, "server"),
    "1.5 kB br · server",
  );
  assert.equal(
    formatImportSize(
      { ...result, side_effects: true },
      { display: "standard", compression: "gzip", showWarnings: true },
      "server",
    ),
    "1.8 kB gz · 5.3 kB min · server",
  );
});

test("formatImportSize labels declaration-only packages as types only", () => {
  assert.equal(
    formatImportSize(
      {
        ...result,
        raw_bytes: 0,
        minified_bytes: 0,
        gzip_bytes: 0,
        brotli_bytes: 0,
        zstd_bytes: 0,
        diagnostics: [
          {
            stage: "types_only",
            message: "package contains declarations only; zero runtime cost",
            details: [],
          },
        ],
      },
      { display: "minimal", compression: "brotli", showWarnings: true },
    ),
    "0 B br · types only",
  );
});
