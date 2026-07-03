import assert from "node:assert/strict";
import test from "node:test";
import { compareImportItemsForResults } from "../../src/ui/compareImportItems.js";
import type { ImportResult } from "../../src/ipc/protocol.js";

const result = (specifier: string, brotliBytes: number, error: string | null = null): ImportResult => ({
  specifier,
  raw_bytes: brotliBytes + 20,
  minified_bytes: brotliBytes + 10,
  gzip_bytes: brotliBytes + 5,
  brotli_bytes: brotliBytes,
  zstd_bytes: brotliBytes + 2,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error,
  diagnostics: [],
});

test("compareImportItemsForResults sorts successful imports by Brotli size", () => {
  assert.deepEqual(
    compareImportItemsForResults([
      result("large-lib", 1500),
      result("small-lib", 500),
    ]),
    {
      items: [
        {
          label: "small-lib: 500 B br",
          detail: "510 B min · 505 B gz · 502 B zstd",
        },
        {
          label: "large-lib: 1.5 kB br",
          detail: "1.5 kB min · 1.5 kB gz · 1.5 kB zstd",
        },
      ],
    },
  );
});

test("compareImportItemsForResults reports no daemon and all-error results", () => {
  assert.deepEqual(compareImportItemsForResults(null), {
    items: [],
    warning: "ImportLens daemon did not return comparison results.",
  });
  assert.deepEqual(compareImportItemsForResults([result("broken-lib", 0, "failed")]), {
    items: [],
    warning: "ImportLens could not compute any comparison results.",
  });
});
