import assert from "node:assert/strict";
import test from "node:test";
import type { WorkspaceReportRow, WorkspaceReportSummary } from "../../src/ipc/protocol.js";
import { workspaceReportHtml } from "../../src/ui/reportContent.js";

const summary = (overrides: Partial<WorkspaceReportSummary> = {}): WorkspaceReportSummary => ({
  importCount: 3,
  combinedImportCostBrotliBytes: 120_000,
  lowConfidenceCount: 0,
  mediumConfidenceCount: 0,
  conservativeCount: 0,
  budgetViolationCount: 0,
  duplicateImports: [],
  sharedModules: [],
  treemap: [],
  ...overrides,
});

const row = (overrides: Partial<WorkspaceReportRow> = {}): WorkspaceReportRow => ({
  packageName: "react",
  specifier: "react",
  sourceFile: "src/app.tsx",
  line: 1,
  runtime: "component",
  minifiedBytes: 20_000,
  gzipBytes: 8_000,
  brotliBytes: 6_000,
  zstdBytes: 7_000,
  sharedBytes: 0,
  confidence: "high",
  confidenceReasons: "",
  topModules: "index.js (6000 B)",
  warning: "",
  moduleContributions: [],
  ...overrides,
});

// The arithmetic was never wrong; the WORD was. "Total Brotli" is read as "what my project ships",
// and it is a sum of independent Import Costs that counts a dependency imported in fifty files fifty
// times (ADR-0004). Deduplicating it would need the project-level bundle model this product declines
// to have — so the number stays and the label tells the truth about it.
test("the report headline is a Combined Import Cost, never a total", () => {
  const html = workspaceReportHtml({ rows: [row()], summary: summary() });

  assert.match(html, /Combined Import Cost<strong>120\.0 kB<\/strong>/u);
  assert.doesNotMatch(
    html,
    /Total Brotli/u,
    "a sum of per-import costs is not a total, and the word is the defect",
  );
});

test("the report says a dependency is counted at every site, and a named+default import twice", () => {
  const html = workspaceReportHtml({ rows: [row()], summary: summary() });

  assert.match(
    html,
    /counted at every site/u,
    "the reader must be told what the sum double-counts",
  );
  assert.match(
    html,
    /import React, \{ useState \} from &quot;react&quot; is two imports/u,
    "and the more surprising half: one statement, one specifier, TWO imports, counted twice",
  );
});

test("the duplicate-imports table reports each group's combined import cost", () => {
  const html = workspaceReportHtml({
    rows: [row()],
    summary: summary({
      duplicateImports: [
        {
          specifier: "react",
          count: 3,
          combinedImportCostBrotliBytes: 18_000,
          sourceFiles: ["src/a.tsx", "src/b.tsx"],
        },
      ],
    }),
  });

  // react in three files genuinely DOES have a combined import cost of three Reacts — that is the
  // panel's point, and by this label the column is finally correct.
  assert.match(html, /<th>Combined Import Cost<\/th>/u);
  assert.match(html, /<td>18\.0 kB<\/td>/u);
});

// The same lie, one table below the one the headline fix relabelled. `react-dom/index.js` is 100 kB
// and three imports reach it; the daemon added it up once per importing row and this table printed
// "Total Bytes: 300 kB". The module is 100 kB. It is REACHED BY 3 imports. The sum across those
// sites is a Combined Import Cost — an upper bound — and it is never a total (ADR-0004).
test("the shared-modules table gives the module its own size and never calls the sum a total", () => {
  const html = workspaceReportHtml({
    rows: [row()],
    summary: summary({
      sharedModules: [
        {
          modulePath: "node_modules/react-dom/index.js",
          basename: "index.js",
          count: 3,
          moduleBytes: 100_000,
          combinedImportCostBytes: 300_000,
          specifiers: ["react-dom", "react-dom/client", "react-dom/server"],
          vendored: false,
        },
      ],
    }),
  });

  assert.doesNotMatch(
    html,
    /Total Bytes/u,
    "a module counted once per importing site is not that module's total",
  );
  assert.match(
    html,
    /<td>index\.js<\/td><td>3<\/td><td>100\.0 kB<\/td><td>300\.0 kB<\/td>/u,
    "the module is 100 kB, reached by 3 imports, and the 300 kB is the sum across those sites",
  );
  assert.match(html, /<th>Module Bytes<\/th><th>Combined Import Cost<\/th>/u);
  assert.match(
    html,
    /reached by more than one import/u,
    "the table must say what it is showing, beside the number",
  );
});

/**
 * The note has to be reconcilable with the two numbers beside it. `moduleBytes` is a **max** across
 * the builds that reached the module — the daemon pins it
 * (`a_module_measured_differently_by_two_builds_reports_its_largest_contribution`: 900 B in one
 * build, 400 B in another → Module Bytes 900, Combined 1.3 kB, Imports 2) — and the note told the
 * reader Module Bytes was "what the module costs at a single site" and Combined "counts it once per
 * site", which reads as 900 × 2 = 1,800.
 *
 * The numbers were right. The sentence explaining them was false, and a reader who trusts the
 * sentence leaves with a number the product never rendered.
 */
test("the shared-module note explains the numbers the table actually renders", () => {
  const html = workspaceReportHtml({
    rows: [],
    summary: summary({
      sharedModules: [
        {
          modulePath: "node_modules/shared/util.js",
          basename: "util.js",
          count: 2,
          // Two builds tree-shook the same module differently: 900 B and 400 B.
          moduleBytes: 900,
          combinedImportCostBytes: 1_300,
          specifiers: ["a", "b"],
          vendored: true,
        },
      ],
    }),
  });

  assert.match(html, /<td>util\.js<\/td><td>2<\/td><td>900 B<\/td><td>1\.3 kB<\/td>/u);
  assert.doesNotMatch(
    html,
    /costs at a single site/u,
    "Module Bytes is the LARGEST contribution across the builds, not a per-site constant - saying \
otherwise invites the reader to multiply it by the import count and land on 1,800",
  );
  assert.match(
    html,
    /the largest contribution across the builds that reached it/u,
    "the note must say what Module Bytes is",
  );
  assert.match(
    html,
    /need not be Module Bytes × Imports/u,
    "and it must say, in as many words, that the two columns do not multiply",
  );
});
