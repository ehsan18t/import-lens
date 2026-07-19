import assert from "node:assert/strict";
import test from "node:test";
import type { DetectedImport, ImportAnalysisItem, ImportResult } from "../../src/ipc/protocol.js";
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

const origin = { line: 0, character: 0 };

const detected = (specifier: string): DetectedImport => ({
  specifier,
  packageName: specifier,
  named: [],
  importKind: "default",
  syntax: "static",
  runtime: "component",
  line: 0,
  quoteEnd: origin,
  specifierRange: { start: origin, end: origin },
  statementRange: { start: origin, end: origin },
});

/** What the daemon returns for an import it analysed, measured or not. */
const analysed = (importResult: ImportResult): ImportAnalysisItem => ({
  detected: detected(importResult.specifier),
  status: "ready",
  result: importResult,
});

/** Analysed, but no result at all — not installed, unresolvable. Channel 2. */
const noResult = (specifier: string, message: string): ImportAnalysisItem => ({
  detected: detected(specifier),
  status: "missing",
  message,
});

test("compareImportItemsForResults sorts successful imports by Brotli size", () => {
  assert.deepEqual(
    compareImportItemsForResults(
      ["large-lib", "small-lib"],
      [analysed(result("large-lib", 1500)), analysed(result("small-lib", 500))],
    ),
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
      comparedCount: 2,
      excludedCount: 0,
    },
  );
});

test("compareImportItemsForResults reports no daemon and all-error results", () => {
  assert.deepEqual(compareImportItemsForResults(["a"], null), {
    items: [],
    comparedCount: 0,
    excludedCount: 1,
    warning: "Import Lens daemon did not return comparison results.",
  });

  // When nothing is comparable there is no pick to open, so the reason has to travel in the warning.
  assert.deepEqual(
    compareImportItemsForResults(["broken-lib"], [analysed(unmeasured("broken-lib", "failed"))]),
    {
      items: [],
      comparedCount: 0,
      excludedCount: 1,
      warning: "Import Lens could not compare any of these imports: broken-lib (failed).",
    },
  );
});

// The comparison ranks candidates by size and the user picks the cheapest. An import with no size
// is not the cheapest candidate — it is not a candidate. It used to be filtered by `!result.error`,
// which a fabricated size passed, so the degraded package sorted to the top of the list and the
// command recommended it.
test("compareImportItemsForResults ranks only the imports it has a size for", () => {
  const { items, comparedCount } = compareImportItemsForResults(
    ["large-lib", "broken-lib", "small-lib"],
    [
      analysed(result("large-lib", 1500)),
      analysed(unmeasured("broken-lib", "engine build did not complete")),
      analysed(result("small-lib", 500)),
    ],
  );

  assert.equal(comparedCount, 2);
  assert.deepEqual(items.slice(0, 2), [
    { label: "small-lib: 500 B br", detail: "510 B min · 505 B gz · 502 B zstd" },
    { label: "large-lib: 1.5 kB br", detail: "1.5 kB min · 1.5 kB gz · 1.5 kB zstd" },
  ]);
});

// A comparison assembled from half-measured imports is worse than an honest "comparison failed"
// (FR-004b). The user asked about these packages; a row missing with no reason reads as "it wasn't
// competitive", not "we could not size it".
test("every requested specifier that cannot be ranked is disclosed with its reason", () => {
  const { items, comparedCount, excludedCount } = compareImportItemsForResults(
    ["lodash", "./local", "not-installed", "broken"],
    [
      analysed(result("lodash", 24_000)),
      noResult("not-installed", "package not installed"),
      analysed(unmeasured("broken", "engine build did not complete")),
    ],
  );

  assert.equal(comparedCount, 1);
  assert.equal(excludedCount, 3, "the daemon pre-filter, the no-result and the no-size channels");
  assert.deepEqual(items, [
    { label: "lodash: 24.0 kB br", detail: "24.0 kB min · 24.0 kB gz · 24.0 kB zstd" },
    { label: "3 not compared", separator: true },
    { label: "./local", detail: "Not compared: not a package import Import Lens can size" },
    { label: "not-installed", detail: "Not compared: package not installed" },
    { label: "broken", detail: "Not compared: engine build did not complete" },
  ]);
});

// Channel 1 alone: the specifier never reaches the response, because the daemon filters it out
// before analysis. Nothing extension-side ever knew it was requested except the request itself.
test("a specifier the daemon never analysed is still disclosed", () => {
  const { items, excludedCount } = compareImportItemsForResults(
    ["react", "./local"],
    [analysed(result("react", 1000))],
  );

  assert.equal(excludedCount, 1);
  assert.deepEqual(items.at(-1), {
    label: "./local",
    detail: "Not compared: not a package import Import Lens can size",
  });
});
