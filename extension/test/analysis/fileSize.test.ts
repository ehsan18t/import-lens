import assert from "node:assert/strict";
import test from "node:test";
import { formatCurrentFileSizeSummary } from "../../src/analysis/fileSize.js";
import type { FileSizeResponse } from "../../src/ipc/protocol.js";

const response = (imports = 2): FileSizeResponse => ({
  version: 3,
  request_id: 1,
  raw_bytes: 12000,
  minified_bytes: 5300,
  gzip_bytes: 1800,
  brotli_bytes: 1500,
  zstd_bytes: 1600,
  imports: Array.from({ length: imports }, (_, index) => ({
    specifier: `pkg-${index}`,
    raw_bytes: 100,
    minified_bytes: 80,
    gzip_bytes: 70,
    brotli_bytes: 60,
    zstd_bytes: 65,
    cache_hit: false,
    side_effects: false,
    truly_treeshakeable: true,
    is_cjs: false,
    error: null,
    diagnostics: [],
  })),
  error: null,
  diagnostics: [],
});

test("formatCurrentFileSizeSummary formats current file totals with selected compression", () => {
  assert.equal(
    formatCurrentFileSizeSummary(response(), "brotli"),
    "Current file: 1.5 kB br · 5.3 kB min · 2 imports",
  );
  assert.equal(
    formatCurrentFileSizeSummary(response(1), "gzip"),
    "Current file: 1.8 kB gz · 5.3 kB min · 1 import",
  );
});
