import assert from "node:assert/strict";
import test from "node:test";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { compareImportItemsForResults } from "../../src/ui/compareImportItems.js";

const base = (
  specifier: string,
): Omit<
  ImportResult,
  "raw_bytes" | "minified_bytes" | "gzip_bytes" | "brotli_bytes" | "zstd_bytes"
> => ({
  specifier,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
});

const result = (specifier: string, brotliBytes: number): ImportResult => ({
  ...base(specifier),
  raw_bytes: brotliBytes + 20,
  minified_bytes: brotliBytes + 10,
  gzip_bytes: brotliBytes + 5,
  brotli_bytes: brotliBytes,
  zstd_bytes: brotliBytes + 2,
});

/** No size, ever — so it has no place in a ranking OF sizes. */
const unmeasured = (specifier: string, error: string): ImportResult => ({
  ...base(specifier),
  raw_bytes: null,
  minified_bytes: null,
  gzip_bytes: null,
  brotli_bytes: null,
  zstd_bytes: null,
  error,
  unmeasured_stage: "entry_resolution",
});

test("compareImportItemsForResults sorts successful imports by Brotli size", () => {
  assert.deepEqual(
    compareImportItemsForResults([result("large-lib", 1500), result("small-lib", 500)]),
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
    warning: "Import Lens daemon did not return comparison results.",
  });
  assert.deepEqual(compareImportItemsForResults([unmeasured("broken-lib", "failed")]), {
    items: [],
    warning: "Import Lens could not compute any comparison results.",
  });
});

// The comparison ranks candidates by size and the user picks the cheapest. An import with no size
// is not the cheapest candidate — it is not a candidate. It used to be filtered by `!result.error`,
// which a fabricated size passed, so the degraded package sorted to the top of the list and the
// command recommended it.
test("compareImportItemsForResults ranks only the imports it has a size for", () => {
  assert.deepEqual(
    compareImportItemsForResults([
      result("large-lib", 1500),
      unmeasured("broken-lib", "engine build did not complete"),
      result("small-lib", 500),
    ]),
    {
      items: [
        { label: "small-lib: 500 B br", detail: "510 B min · 505 B gz · 502 B zstd" },
        { label: "large-lib: 1.5 kB br", detail: "1.5 kB min · 1.5 kB gz · 1.5 kB zstd" },
      ],
    },
  );
});
