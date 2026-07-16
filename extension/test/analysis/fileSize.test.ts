import assert from "node:assert/strict";
import test from "node:test";
import {
  currentFileSizeReport,
  formatCurrentFileSizeSummary,
} from "../../src/analysis/fileSize.js";
import { bundleImpactHistoryItemForResponse } from "../../src/analysis/history.js";
import type {
  FileSizeDocumentResponse,
  ImportAnalysisItem,
  ImportResult,
} from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

const result = (specifier: string): ImportResult => ({
  specifier,
  raw_bytes: 100,
  minified_bytes: 80,
  gzip_bytes: 70,
  brotli_bytes: 60,
  zstd_bytes: 65,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: ["test fixture confidence"],
  error: null,
  diagnostics: [],
});

const state = (specifier: string, status: ImportAnalysisItem["status"]): ImportAnalysisItem => ({
  detected: detectedImport({ specifier, packageName: specifier }),
  status,
  result: status === "ready" ? result(specifier) : undefined,
});

const response = (overrides: Partial<FileSizeDocumentResponse> = {}): FileSizeDocumentResponse => ({
  version: 7,
  request_id: 1,
  raw_bytes: 12000,
  minified_bytes: 5300,
  gzip_bytes: 1800,
  brotli_bytes: 1500,
  zstd_bytes: 1600,
  imports: [result("pkg-0"), result("pkg-1")],
  states: [state("pkg-0", "ready"), state("pkg-1", "ready")],
  error: null,
  diagnostics: [],
  ...overrides,
});

/**
 * What the daemon answers for a document nobody has sized yet. The command's read is streaming (no
 * `force_fresh`), so `imports` carries only the imports it has already MEASURED — none — while
 * `states` carries every import it detected, and the file's own totals come from the combined build
 * and are perfectly real.
 */
const coldResponse = (): FileSizeDocumentResponse =>
  response({
    imports: [],
    states: [state("pkg-0", "loading"), state("pkg-1", "loading")],
    incomplete: true,
  });

test("formatCurrentFileSizeSummary names the quantity it is showing", () => {
  assert.equal(
    formatCurrentFileSizeSummary(response(), "brotli"),
    "File Cost: 1.5 kB br · 5.3 kB min · 2 imports",
  );
  assert.equal(
    formatCurrentFileSizeSummary(response({ states: [state("pkg-0", "ready")] }), "gzip"),
    "File Cost: 1.8 kB gz · 5.3 kB min · 1 import",
  );
});

test("a file with no runtime package imports has nothing to report", () => {
  assert.deepEqual(currentFileSizeReport(response({ imports: [], states: [] }), "brotli"), {
    kind: "no-imports",
  });
});

/**
 * The cold document — the one the user just opened, and the one they are most likely to run the
 * command on. Gating the report on `imports` told them the file "has no resolvable package imports"
 * while the daemon was sizing it perfectly well: `imports` is empty until the per-import builds
 * land, and `states` is what says whether the file HAS imports. `listener.ts` documents exactly this
 * trap for the status bar; the command was left on `imports`.
 */
test("a cold document reports the size the daemon measured, named as the floor it is", () => {
  const cold = coldResponse();
  const report = currentFileSizeReport(cold, "brotli", {
    // A floor: never recorded, so never compared either.
    current: bundleImpactHistoryItemForResponse(cold, "C:/app/src/index.ts"),
  });

  assert.deepEqual(report, {
    kind: "summary",
    message:
      "File Cost floor: 1.5 kB br · 5.3 kB min · 2 imports · an import that belongs in this file's total was not measured, so the number is a floor and not the file's size",
  });
});

/**
 * **The fifth instance, one command over.** The file's own combined build failed, so the totals are
 * an un-deduplicated sum of the per-import costs. EVERY import is Measured — `incomplete: false`,
 * `error: null`, a size on every one of them — and the command said *"estimate (some imports are not
 * fully measured)"*, which is false about every import in the file.
 *
 * The suffix was keyed on `history.current` being absent, and the store withholds that for a floor
 * and an over-count alike, so `degraded` borrowed `incomplete`'s explanation. It now derives its
 * words from the quantity the daemon actually handed over, like every other surface.
 */
test("a degraded total is named a Combined Import Cost, not an estimate with unmeasured imports", () => {
  const degraded = response({ degraded: true, brotli_bytes: 183_200, minified_bytes: 354_000 });
  const report = currentFileSizeReport(degraded, "brotli");

  assert.deepEqual(report, {
    kind: "summary",
    message:
      "Combined Import Cost: 183.2 kB br · 354.0 kB min · 2 imports · the file's combined build failed, so the number is an un-deduplicated sum of its imports and not the file's size",
  });
  assert.doesNotMatch(
    report.kind === "summary" ? report.message : "",
    /not fully measured/u,
    "every import in this response IS fully measured; the file's own build is what failed",
  );
});

/**
 * `skipped` means "the daemon could not size this import", not "the daemon has not sized it YET".
 * An import still building is not skipped — it is the reason the total is an estimate.
 */
test("only the imports the daemon could not size count as skipped", () => {
  const warm = response({
    states: [state("pkg-0", "ready"), state("pkg-1", "ready"), state("pkg-2", "missing")],
  });
  const current = bundleImpactHistoryItemForResponse(warm, "C:/app/src/index.ts");

  assert.deepEqual(currentFileSizeReport(warm, "brotli", { current }), {
    kind: "summary",
    message: "File Cost: 1.5 kB br · 5.3 kB min · 3 imports · 1 skipped",
  });
});

test("a measured total is compared against the file's previous measurement", () => {
  const measured = response();
  const current = bundleImpactHistoryItemForResponse(measured, "C:/app/src/index.ts");
  const previous = bundleImpactHistoryItemForResponse(
    response({ brotli_bytes: 1200 }),
    "C:/app/src/index.ts",
  );

  assert.deepEqual(currentFileSizeReport(measured, "brotli", { current, previous }), {
    kind: "summary",
    message: "File Cost: 1.5 kB br · 5.3 kB min · 2 imports · +300 B br vs previous",
  });
});
