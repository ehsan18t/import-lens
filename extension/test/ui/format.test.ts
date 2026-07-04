import assert from "node:assert/strict";
import test from "node:test";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { formatImportSizePrimary, importHintTagLabels } from "../../src/ui/format.js";

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

test("formatImportSizePrimary supports minimal, standard, and verbose display modes", () => {
  assert.equal(
    formatImportSizePrimary(result, {
      display: "minimal",
      compression: "brotli",
      showWarnings: true,
    }),
    "1.5 kB br",
  );
  assert.equal(
    formatImportSizePrimary(result, {
      display: "standard",
      compression: "brotli",
      showWarnings: true,
    }),
    "1.5 kB br · 5.3 kB min",
  );
  assert.equal(
    formatImportSizePrimary(result, {
      display: "verbose",
      compression: "brotli",
      showWarnings: true,
    }),
    "1.5 kB br · 1.8 kB gz · 1.6 kB zstd · 5.3 kB min",
  );
});

test("formatImportSizePrimary shows unavailable and applies compression and confidence", () => {
  assert.equal(
    formatImportSizePrimary(
      { ...result, error: "parse failed" },
      { display: "standard", compression: "brotli", showWarnings: true },
    ),
    "Size unavailable",
  );
  assert.equal(
    formatImportSizePrimary(
      { ...result, side_effects: true },
      { display: "minimal", compression: "gzip", showWarnings: true },
    ),
    "1.8 kB gz",
  );
  assert.equal(
    formatImportSizePrimary(
      { ...result, is_cjs: true, confidence: "low" },
      { display: "minimal", compression: "zstd", showWarnings: true },
    ),
    "~1.6 kB zstd",
  );
  assert.equal(
    formatImportSizePrimary(
      { ...result, confidence: "low" },
      { display: "standard", compression: "brotli", showWarnings: true },
    ),
    "~1.5 kB br · 5.3 kB min",
  );
});

test("importHintTagLabels reports server, CJS, and warning-visibility tags", () => {
  assert.deepEqual(importHintTagLabels(result, true, "component"), []);
  assert.deepEqual(importHintTagLabels(result, true, "server"), ["server"]);
  assert.deepEqual(importHintTagLabels({ ...result, is_cjs: true }, true, "component"), ["CJS"]);
  assert.deepEqual(importHintTagLabels({ ...result, is_cjs: true }, false, "component"), []);
  assert.deepEqual(importHintTagLabels({ ...result, is_cjs: true }, true, "server"), [
    "server",
    "CJS",
  ]);
});

test("declaration-only packages report zero primary bytes and a types-only tag", () => {
  const typesOnly: ImportResult = {
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
  };

  assert.equal(
    formatImportSizePrimary(typesOnly, {
      display: "minimal",
      compression: "brotli",
      showWarnings: true,
    }),
    "0 B br",
  );
  assert.deepEqual(importHintTagLabels(typesOnly, true, "component"), ["types only"]);
});
