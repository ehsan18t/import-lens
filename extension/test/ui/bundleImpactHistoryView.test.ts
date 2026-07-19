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
  imprecise: false,
  ...overrides,
});

test("bundleImpactHistoryHtml renders a script-free SVG history panel", () => {
  const html = bundleImpactHistoryHtml([
    historyItem({ fileName: "src/<unsafe>&file.ts", brotliBytes: 1200 }),
    historyItem({ timestamp: Date.UTC(2026, 0, 1), brotliBytes: 800 }),
  ]);

  assert.match(html, /<svg role="img" aria-label="File Cost trend/u);
  assert.doesNotMatch(html, /<script/iu);
  assert.match(html, /src\/&lt;unsafe&gt;&amp;file\.ts/u);
  assert.match(html, /1\.2 kB/u);
});

const polylinePoints = (html: string): string[][] =>
  [...html.matchAll(/<polyline points="([^"]*)"/gu)].map((match) =>
    (match[1] ?? "").split(" ").filter(Boolean),
  );

// A polyline is a claim of continuity. Joining app.ts to util.ts to app.ts drew a V-shaped
// crash-and-recovery that happened to no file. The old suite fed two filenames into this renderer
// and asserted only that the SVG existed, so it passed the buggy version too.
test("the chart draws one line per file and never joins two files", () => {
  const html = bundleImpactHistoryHtml([
    historyItem({ fileName: "src/app.ts", timestamp: 1_000, brotliBytes: 900 }),
    historyItem({ fileName: "src/util.ts", timestamp: 2_000, brotliBytes: 400 }),
    historyItem({ fileName: "src/app.ts", timestamp: 3_000, brotliBytes: 1200 }),
    historyItem({ fileName: "src/util.ts", timestamp: 4_000, brotliBytes: 450 }),
  ]);

  const polylines = polylinePoints(html);

  assert.equal(polylines.length, 2, "one polyline per distinct file, not one across all of them");
  assert.deepEqual(
    polylines.map((points) => points.length),
    [2, 2],
    "each file's line joins only its own two measurements",
  );
  assert.match(html, /aria-label="File Cost trend, one line per file, for 2 files/u);
});

test("a single file still renders one line", () => {
  const html = bundleImpactHistoryHtml([
    historyItem({ fileName: "src/app.ts", timestamp: 1_000 }),
    historyItem({ fileName: "src/app.ts", timestamp: 2_000 }),
  ]);

  assert.equal(polylinePoints(html).length, 1);
  assert.match(html, /aria-label="File Cost trend for src\/app\.ts"/u);
});

// (t - oldest) / span is NaN when every measurement shares a timestamp, and an SVG with
// points="NaN,NaN" renders nothing at all rather than failing loudly.
test("measurements sharing a timestamp still plot", () => {
  const html = bundleImpactHistoryHtml([
    historyItem({ fileName: "src/app.ts", timestamp: 5_000, brotliBytes: 900 }),
    historyItem({ fileName: "src/util.ts", timestamp: 5_000, brotliBytes: 400 }),
  ]);

  assert.doesNotMatch(html, /NaN/u);
  assert.equal(polylinePoints(html).length, 2);
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
