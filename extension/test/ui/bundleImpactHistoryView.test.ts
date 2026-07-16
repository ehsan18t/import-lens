import assert from "node:assert/strict";
import test from "node:test";
import type { BundleImpactHistoryItem } from "../../src/analysis/history.js";
import { bundleImpactHistoryHtml } from "../../src/ui/bundleImpactHistoryView.js";

const historyItem = (
  overrides: Partial<BundleImpactHistoryItem> = {},
): BundleImpactHistoryItem => ({
  timestamp: Date.UTC(2026, 0, 2, 3, 4, 5),
  fileName: "src/app.ts",
  rawBytes: 9000,
  minifiedBytes: 3000,
  gzipBytes: 1200,
  brotliBytes: 900,
  zstdBytes: 1000,
  importCount: 3,
  ...overrides,
});

test("bundleImpactHistoryHtml renders a script-free SVG history panel", () => {
  const html = bundleImpactHistoryHtml([
    historyItem({ fileName: "src/<unsafe>&file.ts", brotliBytes: 1200 }),
    historyItem({ timestamp: Date.UTC(2026, 0, 1), brotliBytes: 800 }),
  ]);

  assert.match(html, /<svg role="img" aria-label="File Cost trend"/u);
  assert.doesNotMatch(html, /<script/iu);
  assert.match(html, /src\/&lt;unsafe&gt;&amp;file\.ts/u);
  assert.match(html, /1\.2 kB/u);
});

// Every row is one file's **File Cost** — the daemon's single combined build over that file's
// imports (ADR-0004). Nothing here is summed across files, and nothing here is what the project
// ships. The column said "Brotli", which names a compression format and not a quantity.
test("the history panel names the quantity it plots", () => {
  const html = bundleImpactHistoryHtml([historyItem()]);

  assert.match(html, /<th>File Cost \(br\)<\/th>/u);
  assert.match(html, /Latest File Cost/u);
  assert.match(
    html,
    /one bundle over that file&#39;s imports/u,
    "the panel must say what the number is, beside the number",
  );
});
