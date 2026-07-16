import assert from "node:assert/strict";
import test from "node:test";
import { lineAt, sourceFiles, stripComments } from "../source-scan.mjs";

// GUARD for the unit of measurement (docs/adr/0004-import-lens-measures-imports-not-bundles.md).
//
// Import Lens measures IMPORTS, not bundles. Three quantities, and conflating them IS the bug:
//
//   Import Cost           what ONE import costs, alone, against an otherwise-empty app.
//   Combined Import Cost  the SUM of independent Import Costs. Counts a shared dependency at EVERY
//                         site. An UPPER BOUND. It ranks and it apportions blame. Never a size.
//   File Cost             ONE bundle over a file's imports, so a module reached by two of them is
//                         counted once. A real quantity.
//
// The defect keeps arriving in one shape: **a sum of per-import figures given a "total"-flavoured
// name**, which every reader takes to mean *what my project ships*. It has now been found four
// times — the report headline (`total_brotli_bytes`, rendered "Total Brotli"), the per-file budget
// (re-derived by summing per-import costs: a 55 kB file under a 60 kB budget warned at 200.0 kB),
// the Shared Modules table (`total_bytes`, rendered "Total Bytes": a 100 kB module reached by three
// imports rendered as **300 kB**), and the package.json section summary (a bare byte count beside
// "3/3 measured"). Each was fixed one surface at a time, and the next one was found in the panel
// nobody had looked at yet.
//
// ## WHAT THIS GUARD ACTUALLY COVERS — and, precisely, what it does not
//
// **It is a NAMING guard. It cannot see arithmetic.** No regex can decide whether a `u64` is a sum
// of per-import costs; that is a question about where the number came from, and answering it needs
// the call graph. What a regex CAN do is ban the word that turns an honest upper bound into a lie,
// in the two places a figure gets its name:
//
//   1. **The declaration.** A numeric struct field, interface field, or `const`/`let` binding whose
//      name pairs a total-word (total, overall, aggregate, grand, sum) with a size-word.
//   2. **The rendered label.** The text inside `<th>…</th>` and `<div class="metric">…` in the
//      extension's webviews — the two markup positions where the report and the history panel name
//      a column and a headline.
//
// Everything else is out of reach, and pretending otherwise would be worse than no guard at all:
//
//   - **A sum with no name passes.** `rows.reduce((sum, row) => sum + row.brotliBytes, 0)` inlined
//     straight into a template literal under a `<th>Bytes</th>` is invisible here. That is the
//     residual risk, and it is not small.
//   - **A total by a euphemism passes.** `everythingBytes`, `wholeCost`, `projectSize`.
//   - **Prose is not scanned.** A diagnostic must be able to SAY "this total is not the file's size"
//     — `importlens check` says exactly that — so free text is never an offence.
//
// The enforcement that actually holds is the type of the number and the tests around it: the report
// summary's field is `combined_import_cost_brotli_bytes` and the shared-module group carries BOTH
// `module_bytes` and `combined_import_cost_bytes`, each asserted by a rendering test that pins the
// verbatim output (`reportContent.test.ts`, `report/model.rs`). This is the tripwire, not the lock.
//
// ## THE HONEST NUMBER: 18 of 22, and the guard was WEAKER THAN ITS OWN ADVERTISED COVERAGE
//
// It claimed "12 of 16". That claim was measured against a corpus written by the same hand as the
// matchers, and it **overstated the guard's reach in the vocabulary this product actually uses on
// screen**. Six spellings were then planted independently, and the guard scored **0 of 6**:
//
//   - `totalBr`, `overallGz`, `grandKb` — literal sums of per-import figures under total-flavoured
//     names, planted in `workspaceReportHtml` and returning GREEN. `sizeWords` knew `brotli` and
//     `gzip` and did not know `br`, `gz`, `min` or `kb` — **the words the UI renders** ("141.2 kB
//     br", "· gz", "· min"). A guard whose vocabulary is not the product's vocabulary catches the
//     spellings its author thought of and no others.
//   - `<h2>Total Project Size</h2>`, `<caption>Total Bytes</caption>`, `<td>Total Brotli</td>` — the
//     label scan read `<th>` and `class="metric">` and nothing else. A heading names a figure every
//     bit as loudly as a column header does.
//
// Both holes are closed and all six are now in the corpus below, which is why the number moved to
// **18 of 22**. That is not 6 points of new strength honestly earned against an unseen set: three of
// the six are declarations the widened `sizeWords` was widened FOR. The four it still cannot see are
// named below with reasons, and they are the same four as before — the structural ones. A guard that
// looks stronger than it is buys false confidence, so the number is computed, asserted, and stated
// here rather than claimed.

const guardFile = "scripts/test/import-cost-naming-guards.test.mjs";

const allFiles = sourceFiles().filter(
  // This file quotes every banned spelling in order to ban it.
  (file) => file.path !== guardFile,
);

/** The words that turn an upper bound into a claim about what the project ships. */
const totalWords = new Set([
  "total",
  "totals",
  "overall",
  "aggregate",
  "aggregated",
  "grand",
  "sum",
  "sums",
]);

/**
 * The words that make a name a name for BYTES — **including the abbreviations the product actually
 * renders**, which this list did not have.
 *
 * The UI's own labels are `141.2 kB br`, `· gz`, `· min`. So `totalBr`, `overallGz` and `grandKb`
 * are the names a next author reaches for, they are literal sums of per-import figures under
 * total-flavoured names — precisely the shape this guard promises to catch — and all three were
 * planted in `workspaceReportHtml` and came back GREEN. The guard's vocabulary has to be the
 * product's vocabulary, or it only catches the spellings its author happened to think of.
 */
const sizeWords = new Set([
  "byte",
  "bytes",
  "size",
  "sizes",
  "cost",
  "costs",
  "brotli",
  "br",
  "gzip",
  "gz",
  "zstd",
  "minified",
  "min",
  "raw",
  "kb",
  "mb",
]);

/** `totalBrotliBytes` and `TOTAL_SOURCE_BYTES` and `total_bytes` all become the same word list. */
const words = (identifier) =>
  identifier
    .replace(/([a-z0-9])([A-Z])/gu, "$1 $2")
    .split(/[^A-Za-z0-9]+/u)
    .filter(Boolean)
    .map((word) => word.toLowerCase());

const namesATotalOfBytes = (identifier) => {
  const parts = words(identifier);
  return parts.some((word) => totalWords.has(word)) && parts.some((word) => sizeWords.has(word));
};

/**
 * Genuine totals, each of ONE thing that is really added up, with the reason it is not a Combined
 * Import Cost.
 *
 * Keyed by the **type or function the figure lives on**, not by its file: `CacheStatus.total_bytes`
 * is the cache's footprint and is honest, while `DuplicateModuleGroup.total_bytes` was a module
 * counted once per importing site — and both were declared in `ipc/protocol.rs`. A file-level
 * allowlist would have excused the second because of the first, which is exactly how the defect
 * moved from panel to panel: the same word, one struct over.
 *
 * A **real** total sums bytes that exist in one place at one time: the cache's physical footprint on
 * disk, and the source bytes of a single module graph. Neither counts anything twice, and neither is
 * a sum of independent per-import measurements — which is the only thing ADR-0004 forbids.
 */
const allowed = new Set([
  // The cache's own footprint: bytes physically on disk, in one directory, right now. Nothing here
  // is counted twice, because the bytes are all in one place at one time.
  "daemon/src/cache/disk.rs#-.SUMMARY_TOTAL_BYTES",
  "daemon/src/cache/disk.rs#ShardRollup.total_bytes",
  "daemon/src/cache/disk.rs#shard_rollup.total_bytes",
  "daemon/src/cache/disk.rs#rebuild_summary_in_txn.total_bytes",
  "daemon/src/cache/disk.rs#write_pending_inserts.total_bytes",
  "daemon/src/cache/disk.rs#IntoIterator.total_bytes",
  "daemon/src/cache/project.rs#ProjectCacheStatus.total_bytes",
  "daemon/src/cache/project.rs#ProjectCacheStatus.total_size_bytes",
  "daemon/src/cache/project.rs#status_for_root.total_bytes",
  "daemon/src/cache/project.rs#status_for_root.total_size_bytes",
  "daemon/src/ipc/protocol.rs#CacheStatusResponse.total_bytes",
  "daemon/src/ipc/protocol.rs#CacheStatusResponse.total_size_bytes",
  "extension/src/ipc/protocol.ts#CacheStatusResponse.total_bytes",
  "extension/src/ipc/protocol.ts#CacheStatusResponse.total_size_bytes",
  "extension/src/ui/cacheManagerItems.ts#cacheManagerActionItems.totalBytes",
  // The bytes of ONE build: the uncounted assets a single package ships, and the source bytes of a
  // single module graph measured against the graph-source limit (FR-018a).
  "daemon/src/engine/adapter.rs#uncounted_assets_diagnostic.total_bytes",
  "daemon/src/engine/plugin.rs#module_parsed.total_bytes",
]);

/**
 * The type, function, or constant a declaration sits inside — the thing a figure belongs to.
 *
 * Nothing more than "the nearest declaration keyword above it", which is enough: a struct field's
 * container is its struct, and a `let` binding's is the function it is computed in.
 */
const containerAt = (code, index) => {
  const before = code.slice(0, index);
  const containers = [
    ...before.matchAll(
      /\b(?:struct|interface|class|enum|impl|fn|type)\s+([A-Za-z_$][\w$]*)|\bexport\s+const\s+([A-Za-z_$][\w$]*)/gu,
    ),
  ];
  const last = containers.at(-1);
  return last ? (last[1] ?? last[2]) : "-";
};

const isAllowed = (file, container, identifier) =>
  allowed.has(`${file}#${container}.${identifier}`);

/** A numeric field or binding — where a figure gets its name. Function names are not figures. */
const declarations = (code) => [
  // Rust: `pub total_bytes: u64,` / `total_size_bytes: Option<u64>,`
  ...code.matchAll(
    /(?:^|[{,;]|\bpub\s)\s*([a-z_][a-z0-9_]*)\s*:\s*(?:Option<)?(?:u8|u16|u32|u64|usize|f32|f64)\b/gmu,
  ),
  // Rust: `let total_bytes = …` / `const TOTAL_BYTES: u64 = …`
  ...code.matchAll(/\b(?:let|const|static)\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*[:=]/gu),
  // TypeScript: `totalBytes: number;` / `readonly totalBrotliBytes?: number`
  ...code.matchAll(/(?:^|[{,;])\s*(?:readonly\s+)?([A-Za-z_$][\w$]*)\??\s*:\s*number\b/gmu),
];

/**
 * The markup positions the webviews name a figure in.
 *
 * It scanned `<th>` and `class="metric">` and nothing else, so `<h2>Total Project Size</h2>`, a
 * `<caption>Total Bytes</caption>` over the very table whose header it policed, and a `<td>Total
 * Brotli</td>` all sailed past it. A heading names a figure exactly as loudly as a column header
 * does.
 */
const renderedLabels = (code) => [
  ...code.matchAll(/<(?:th|td|caption|h1|h2|h3|figcaption|legend)>([^<]*)</gu),
  ...code.matchAll(/class="metric">([^<]*)</gu),
];

const labelIsATotal = (label) =>
  words(label).some((word) => totalWords.has(word) && word !== "sum" && word !== "sums");

/**
 * ADR-0004: "No figure the product displays may be named or framed as one [a Bundle Size]." The
 * status bar called the File Cost "Current file bundle size" for the whole life of the product.
 */
const framedAsABundleSize = /bundle\s+size/iu;

const offences = (file) => {
  const code = stripComments(file.text);
  const found = [];

  for (const match of declarations(code)) {
    const identifier = match[1];
    const container = containerAt(code, match.index);

    if (namesATotalOfBytes(identifier) && !isAllowed(file.path, container, identifier)) {
      found.push(
        `${file.path}:${lineAt(code, match.index)} declares \`${container}.${identifier}\``,
      );
    }
  }

  if (!file.path.startsWith("extension/src/")) {
    return found;
  }

  for (const match of renderedLabels(code)) {
    if (labelIsATotal(match[1])) {
      found.push(
        `${file.path}:${lineAt(code, match.index)} renders the label "${match[1].trim()}"`,
      );
    }
  }

  for (const match of code.matchAll(new RegExp(framedAsABundleSize, "giu"))) {
    found.push(`${file.path}:${lineAt(code, match.index)} calls a figure a "${match[0]}"`);
  }

  return found;
};

/**
 * A sum of per-import figures is a **Combined Import Cost** — an upper bound that counts a shared
 * dependency at every site — and it is never a Total. Name it for what it is, or do not show it.
 */
test("no size-bearing figure is declared or rendered as a total", () => {
  const found = allFiles.flatMap((file) => offences(file));

  assert.deepEqual(
    found,
    [],
    `A total-flavoured name on a size is how "300 kB" got printed for a 100 kB module:\n${found.join("\n")}`,
  );
});

// The corpus, and the number. Every spelling the guard claims to catch is caught HERE, computed —
// so weakening a matcher fails this test rather than quietly lowering the coverage.
const caught = [
  // The four real defects, in the spelling each of them actually shipped in.
  { path: "daemon/src/report/model.rs", text: "    pub total_bytes: u64,\n" },
  { path: "daemon/src/ipc/protocol.rs", text: "    pub total_brotli_bytes: u64,\n" },
  { path: "extension/src/ipc/protocol.ts", text: "  totalBytes: number;\n" },
  { path: "extension/src/ui/reportContent.ts", text: "<th>Total Bytes</th>" },
  {
    path: "extension/src/ui/reportContent.ts",
    text: '<div class="metric">Total Brotli<strong>300.0 kB</strong></div>',
  },
  {
    path: "extension/src/ui/packageJsonLabels.ts",
    text: "  const totalBytes = states.reduce((sum, state) => sum + state.bytes, 0);\n",
  },
  // The same idea under the words a next author would reach for.
  { path: "daemon/src/report/model.rs", text: "    pub aggregate_size_bytes: u64,\n" },
  { path: "daemon/src/report/model.rs", text: "        let overall_brotli_bytes = 0u64;\n" },
  { path: "extension/src/ui/report.ts", text: "  readonly grandTotalBytes: number;\n" },
  { path: "extension/src/ui/report.ts", text: "  const sumBytes = rows.reduce(add, 0);\n" },
  { path: "extension/src/ui/reportContent.ts", text: "<th>Total</th>" },
  { path: "extension/src/ui/statusbarText.ts", text: "  return `Current file bundle size`;" },
  // --- Planted independently, in the vocabulary the product RENDERS. The guard scored 0 of 6 on
  // --- these: its `sizeWords` knew `brotli` and `gzip` and not `br`, `gz`, `min` or `kb` — the
  // --- words on screen — and its label scan knew `<th>` and nothing else. A guard whose vocabulary
  // --- is not the product's vocabulary catches the spellings its author thought of, and no others.
  {
    path: "extension/src/ui/reportContent.ts",
    text: "  const totalBr = rows.reduce((sum, row) => sum + row.brotliBytes, 0);\n",
  },
  {
    path: "extension/src/ui/reportContent.ts",
    text: "  const overallGz = rows.reduce((sum, row) => sum + row.gzipBytes, 0);\n",
  },
  {
    path: "extension/src/ui/reportContent.ts",
    text: "  const grandKb = rows.reduce((sum, row) => sum + row.brotliBytes, 0) / 1000;\n",
  },
  { path: "extension/src/ui/reportContent.ts", text: "<h2>Total Project Size</h2>" },
  { path: "extension/src/ui/reportContent.ts", text: "<caption>Total Bytes</caption>" },
  { path: "extension/src/ui/reportContent.ts", text: "<td>Total Brotli</td>" },
];

// What it CANNOT catch, kept beside what it can so the claim stays honest. Every one of these is a
// real way to ship the defect, and none of them is visible to a regex.
const missed = [
  // A sum with no name at all: the arithmetic handed straight to the renderer, under a column
  // header that names a compression format rather than a quantity.
  {
    path: "extension/src/ui/reportContent.ts",
    text: "  return formatBytes(rows.reduce((sum, row) => sum + row.brotliBytes, 0));\n",
  },
  // A total by a euphemism.
  { path: "daemon/src/report/model.rs", text: "    pub everything_bytes: u64,\n" },
  { path: "extension/src/ui/report.ts", text: "  readonly projectSize: number;\n" },
  // A method, not a field: `fn total(&self) -> u64`.
  { path: "daemon/src/report/model.rs", text: "    fn total(&self) -> u64 { self.bytes.sum() }\n" },
];

test("the guard catches exactly the spellings it claims to", () => {
  const escaped = caught.filter((plant) => offences(plant).length === 0);
  const slipped = missed.filter((plant) => offences(plant).length > 0);

  assert.deepEqual(
    escaped.map((plant) => plant.text.trim()),
    [],
    "these are the spellings the guard PROMISES to catch, and it did not",
  );
  assert.deepEqual(
    slipped.map((plant) => plant.text.trim()),
    [],
    "the guard caught something the file says it cannot — raise the claim, do not lower the test",
  );
  assert.equal(caught.length, 18, "the measured catch corpus: 18 of 22 planted spellings");
  assert.equal(missed.length, 4, "and the 4 it cannot see, named in this file with reasons");
});
