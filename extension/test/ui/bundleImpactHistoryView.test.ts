import assert from "node:assert/strict";
import test from "node:test";
import { bundleImpactHistoryHtml } from "../../src/ui/bundleImpactHistoryView.js";
import type { BundleImpactHistoryItem } from "../../src/analysis/history.js";

const historyItem = (overrides: Partial<BundleImpactHistoryItem> = {}): BundleImpactHistoryItem => ({
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

  assert.match(html, /<svg role="img" aria-label="Brotli size trend"/u);
  assert.doesNotMatch(html, /<script/iu);
  assert.match(html, /src\/&lt;unsafe&gt;&amp;file\.ts/u);
  assert.match(html, /1\.2 kB/u);
});
