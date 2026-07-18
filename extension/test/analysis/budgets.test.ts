import assert from "node:assert/strict";
import test from "node:test";
import {
  budgetInsightForState,
  budgetViolationsForStates,
  fileBudgetVerdict,
  sanitizeBudgets,
} from "../../src/analysis/budgets.js";
import type { DocumentFileCost } from "../../src/analysis/fileSize.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import type { ImportResult } from "../../src/ipc/protocol.js";
import { detectedImport, sourceRange } from "../helpers/detectedImport.js";

const detected = (specifier: string, line: number) =>
  detectedImport({
    specifier,
    packageName: specifier,
    line,
    quoteEnd: { line, character: 20 },
    specifierRange: sourceRange(line, 8, 18),
    statementRange: sourceRange(line, 0, 24),
  });

const result = (specifier: string, brotliBytes: number): ImportResult => ({
  specifier,
  raw_bytes: brotliBytes + 100,
  minified_bytes: brotliBytes + 50,
  gzip_bytes: brotliBytes + 20,
  brotli_bytes: brotliBytes,
  zstd_bytes: brotliBytes + 10,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
});

const ready = (specifier: string, brotliBytes: number, line = 0): ImportAnalysisState => ({
  detected: detected(specifier, line),
  status: "ready",
  result: result(specifier, brotliBytes),
});

/** The daemon's File Cost for the document: ONE bundle over all its imports, plus the flags that say whether it is this file's number at all. */
const fileCost = (
  brotliBytes: number,
  flags: Partial<DocumentFileCost> = {},
): DocumentFileCost => ({
  brotliBytes,
  error: null,
  diagnostics: [],
  ...flags,
});

/** Five subpath imports of one package: 40 kB each on their own, most of the graph in common. */
const sharedGraphImports = (): ImportAnalysisState[] =>
  [0, 1, 2, 3, 4].map((index) => ready(`@mui/material/Component${index}`, 40_000, index));

test("sanitizeBudgets accepts positive thresholds and drops invalid values", () => {
  assert.deepEqual(
    sanitizeBudgets({
      perImportBrotliBytes: 1500,
      perFileBrotliBytes: 4000,
      ignored: true,
    }),
    {
      perImportBrotliBytes: 1500,
      perFileBrotliBytes: 4000,
    },
  );
  assert.deepEqual(
    sanitizeBudgets({
      perImportBrotliBytes: -1,
      perFileBrotliBytes: Number.NaN,
    }),
    {},
  );
});

test("budgetViolationsForStates reports per-import violations and the File Cost's file violation", () => {
  const states = [ready("large-lib", 2600, 2), ready("small-lib", 900, 3)];

  assert.deepEqual(
    budgetViolationsForStates(
      states,
      {
        perImportBrotliBytes: 2000,
        perFileBrotliBytes: 3000,
      },
      // The file's own build: 3.4 kB, not the 3.5 kB the two imports sum to. The violation must
      // carry the number the daemon measured, or the diagnostic and the status bar print two
      // different sizes for one file.
      fileCost(3400),
    ).map((violation) => ({
      kind: violation.kind,
      specifier: violation.specifier,
      actualBytes: violation.actualBytes,
      limitBytes: violation.limitBytes,
      line: violation.range.start.line,
    })),
    [
      { kind: "import", specifier: "large-lib", actualBytes: 2600, limitBytes: 2000, line: 2 },
      { kind: "file", specifier: undefined, actualBytes: 3400, limitBytes: 3000, line: 2 },
    ],
  );
});

/**
 * ADR-0004. Five imports sharing most of one graph cost 200 kB as five INDEPENDENT Import Costs —
 * a Combined Import Cost, an upper bound, never a size. The file the daemon actually builds is
 * 55 kB and is inside its budget, which is what the status bar has been showing all along, one line
 * from the diagnostic that called the same file 3x over.
 */
test("the file budget gates on the File Cost, not on a sum of per-import costs", () => {
  const budgets = { perFileBrotliBytes: 60_000 };

  assert.deepEqual(
    budgetViolationsForStates(sharedGraphImports(), budgets, fileCost(55_000)).filter(
      (violation) => violation.kind === "file",
    ),
    [],
    "the file is inside budget; summing the five would have raised a false violation",
  );
  assert.equal(fileBudgetVerdict(budgets, fileCost(55_000)), "within-budget");
});

/**
 * ADR-0006, invariant 5. An incomplete total is a FLOOR (an import that belongs in it contributed no
 * bytes) and a degraded one is an un-deduplicated per-import sum (the file's own combined build
 * failed) — an OVER-count. Neither is the file's size, so neither may produce a verdict: not a
 * violation, and — the half that is easy to forget — not a pass either.
 */
test("an incomplete File Cost is not evaluated: no violation, and no pass", () => {
  const budgets = { perFileBrotliBytes: 60_000 };
  const floor = fileCost(200_000, { incomplete: true });

  assert.deepEqual(
    budgetViolationsForStates(sharedGraphImports(), budgets, floor).filter(
      (violation) => violation.kind === "file",
    ),
    [],
  );
  assert.equal(fileBudgetVerdict(budgets, floor), "not-evaluated");
  assert.equal(
    fileBudgetVerdict(budgets, fileCost(10, { incomplete: true })),
    "not-evaluated",
    "a floor UNDER the budget establishes no pass either: the missing bytes are unknown",
  );
});

test("a degraded File Cost is not evaluated: no violation, and no pass", () => {
  const budgets = { perFileBrotliBytes: 60_000 };
  const overCount = fileCost(200_000, { degraded: true });

  assert.deepEqual(
    budgetViolationsForStates(sharedGraphImports(), budgets, overCount).filter(
      (violation) => violation.kind === "file",
    ),
    [],
    "a per-import sum is a different quantity (ADR-0004); it cannot fail a budget it never measured",
  );
  assert.equal(fileBudgetVerdict(budgets, overCount), "not-evaluated");
  assert.equal(fileBudgetVerdict(budgets, fileCost(10, { degraded: true })), "not-evaluated");
});

test("an absent File Cost is not evaluated: the file budget waits for the number", () => {
  const budgets = { perFileBrotliBytes: 60_000 };

  assert.deepEqual(
    budgetViolationsForStates(sharedGraphImports(), budgets).filter(
      (violation) => violation.kind === "file",
    ),
    [],
  );
  assert.equal(fileBudgetVerdict(budgets, undefined), "not-evaluated");
});

test("the per-import budget is judged per import, File Cost or not", () => {
  const states = [ready("large-lib", 2600, 2), ready("small-lib", 900, 3)];

  assert.deepEqual(
    budgetViolationsForStates(states, { perImportBrotliBytes: 2000 }).map(
      (violation) => violation.specifier,
    ),
    ["large-lib"],
  );
});

test("a transient asset floor produces no per-import budget verdict", () => {
  const state = ready("asset-lib", 2600, 2);
  state.result?.diagnostics.push({
    stage: "asset_io",
    message: "a stylesheet dependency could not be read",
    details: [],
  });

  assert.equal(
    budgetInsightForState(state, { perImportBrotliBytes: 2000 }),
    null,
    "a missing asset can move the import to either side of the threshold on retry",
  );
  assert.deepEqual(
    budgetViolationsForStates([state], { perImportBrotliBytes: 2000 }),
    [],
    "neither an inline badge nor a Problems-panel verdict may be drawn from the floor",
  );
});

test("an imprecise asset upper bound produces no budget verdict", () => {
  const state = ready("asset-lib", 2600, 2);
  state.result?.diagnostics.push({
    stage: "imprecise_assets",
    message: "stylesheets were measured separately, so this size may read high",
    details: [],
  });
  const upperBound = fileCost(3400, {
    diagnostics: [
      {
        stage: "imprecise_assets",
        message: "stylesheets were measured separately, so this size may read high",
        details: [],
      },
    ],
  });
  const budgets = { perImportBrotliBytes: 2000, perFileBrotliBytes: 3000 };

  assert.equal(
    budgetInsightForState(state, budgets),
    null,
    "a disclosed upper bound can cross the threshold only because CSS artifacts were split",
  );
  assert.deepEqual(
    budgetViolationsForStates([state], budgets, upperBound),
    [],
    "neither the import nor file upper bound may produce a false failure",
  );
  assert.equal(fileBudgetVerdict(budgets, upperBound), "not-evaluated");
  assert.equal(
    fileBudgetVerdict(budgets, fileCost(10, { diagnostics: upperBound.diagnostics })),
    "not-evaluated",
    "the same inexact number cannot establish a pass when it happens to be below the limit",
  );
});

test("budgetInsightForState adds distinct inline and hover text for import budget violations", () => {
  assert.deepEqual(
    budgetInsightForState(ready("large-lib", 2600), { perImportBrotliBytes: 2000 }),
    {
      label: "over budget",
      tooltip: "Budget: large-lib is 2.6 kB br, over the per-import budget of 2.0 kB br.",
    },
  );
  assert.equal(
    budgetInsightForState(ready("small-lib", 900), { perImportBrotliBytes: 2000 }),
    null,
  );
});
