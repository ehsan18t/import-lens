import assert from "node:assert/strict";
import test from "node:test";
import { formatImportSize } from "../../src/ui/format.js";

const result = {
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
  error: null,
  diagnostics: [],
};

test("formatImportSize supports minimal, standard, and verbose display modes", () => {
  assert.equal(formatImportSize(result, { display: "minimal", compression: "brotli", showWarnings: true }), "1.5 kB");
  assert.equal(formatImportSize(result, { display: "standard", compression: "brotli", showWarnings: true }), "1.5 kB br · 5.3 kB min");
  assert.equal(formatImportSize(result, { display: "verbose", compression: "brotli", showWarnings: true }), "1.5 kB br · 1.8 kB gz · 1.6 kB zstd · 5.3 kB min");
});

test("formatImportSize shows unavailable and warning states", () => {
  assert.equal(formatImportSize({ ...result, error: "parse failed" }, { display: "standard", compression: "brotli", showWarnings: true }), "unavailable");
  assert.equal(formatImportSize({ ...result, side_effects: true }, { display: "minimal", compression: "gzip", showWarnings: true }), "1.8 kB · approximate");
  assert.equal(formatImportSize({ ...result, is_cjs: true }, { display: "minimal", compression: "zstd", showWarnings: true }), "1.6 kB · CJS");
});
