import assert from "node:assert/strict";
import test from "node:test";
import {
  bundleImpactHistoryItemForResponse,
  importCostHistoryItem,
} from "../../src/analysis/history.js";
import { importCostHistoryItemsForStates } from "../../src/analysis/insights.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import {
  isDurableFileSize,
  isDurableImportResult,
  transientEngineStages,
} from "../../src/analysis/transience.js";
import type {
  FileSizeDocumentResponse,
  ImportAnalysisItem,
  ImportResult,
} from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

const measured: ImportResult = {
  specifier: "lodash-es",
  raw_bytes: 17_550,
  minified_bytes: 9_000,
  gzip_bytes: 3_000,
  brotli_bytes: 2_500,
  zstd_bytes: 2_800,
  cache_hit: false,
  side_effects: false,
  truly_treeshakeable: true,
  is_cjs: false,
  confidence: "high",
  confidence_reasons: [],
  error: null,
  diagnostics: [],
};

/**
 * **Unmeasured** (ADR-0006). The build could not answer, and the stage says whether that is a fact
 * about the package or about this moment's scheduling. There is no size — which is the change: the
 * daemon used to substitute a static one, so the result reached every store in the system carrying
 * `error: null` and a perfectly plausible byte count, and every store took it.
 */
const unmeasured = (stage: string): ImportResult => ({
  ...measured,
  raw_bytes: null,
  minified_bytes: null,
  gzip_bytes: null,
  brotli_bytes: null,
  zstd_bytes: null,
  confidence: "low",
  truly_treeshakeable: false,
  error: "engine build did not complete",
  unmeasured_stage: stage,
  diagnostics: [{ stage, message: "engine build did not complete", details: [] }],
});

/**
 * The OTHER transient shape, and the one that still carries real bytes: a build that SUCCEEDED,
 * whose full-package comparison build then timed out. Its sizes are honest; its
 * `truly_treeshakeable: false` is fabricated by a scheduling accident, so it is still not durable.
 */
const comparisonDegraded = (stage: string): ImportResult => ({
  ...measured,
  truly_treeshakeable: false,
  diagnostics: [{ stage, message: "full-package comparison build failed", details: [] }],
});

// A DETERMINISTIC failure is a fact about the code: it will fail the same way next time, and the
// daemon caches it for exactly that reason.
const failedToParse: ImportResult = unmeasured("parse");

const stateFor = (specifier: string, result: ImportResult): ImportAnalysisState => ({
  detected: detectedImport({ specifier, packageName: specifier, named: [], importKind: "default" }),
  status: "ready",
  result,
});

const itemFor = (specifier: string): ImportAnalysisItem => ({
  detected: detectedImport({ specifier, packageName: specifier, named: [], importKind: "default" }),
  status: "ready",
});

const fileSize = (overrides: Partial<FileSizeDocumentResponse> = {}): FileSizeDocumentResponse => ({
  version: 7,
  request_id: 1,
  raw_bytes: 1_000,
  minified_bytes: 500,
  gzip_bytes: 200,
  brotli_bytes: 180,
  zstd_bytes: 190,
  imports: [],
  states: [],
  error: null,
  diagnostics: [],
  ...overrides,
});

/**
 * **Property** over EVERY transient stage the daemon declares × every durable store the extension
 * owns. A stage added to `transientEngineStages` that some store forgets to gate on fails here.
 *
 * The extension's stores are the worst place in the system for a fabricated number to land: they
 * have no TTL, no cache generation, and one row per identity, so the value does not go stale — it
 * becomes that import's permanent baseline, and every later trend is measured against a number
 * that never happened.
 */
test("no durable store takes a transient outcome, in any of its shapes", () => {
  assert.ok(transientEngineStages.length > 0, "there must be at least one transient stage");

  for (const stage of transientEngineStages) {
    assert.equal(
      isDurableImportResult(unmeasured(stage)),
      false,
      `an import unmeasured by \`${stage}\` has no size to record and no reason to be trusted`,
    );
    assert.equal(
      isDurableImportResult(comparisonDegraded(stage)),
      false,
      `a measurement whose comparison build hit \`${stage}\` carries a fabricated tree-shake verdict`,
    );

    assert.deepEqual(
      importCostHistoryItemsForStates(
        [stateFor("lodash-es", unmeasured(stage)), stateFor("dayjs", measured)],
        1_000,
      ).map((item) => item.specifier),
      ["dayjs"],
      `a \`${stage}\` row would replace lodash-es's real baseline for good`,
    );

    // **The row CONSTRUCTOR**, not just the filter in front of it. `ImportCostHistoryItem` is five
    // sizes and an identity: once one exists, nothing downstream can tell how it was measured, so
    // `recordImportCostHistory` cannot re-derive whether it was safe to keep. The gate has to be
    // here, or the store is takeable by anyone who builds a row directly — which is the whole shape
    // of this defect: a predicate the caller was supposed to remember.
    for (const shape of [unmeasured(stage), comparisonDegraded(stage)]) {
      assert.equal(
        importCostHistoryItem(
          detectedImport({
            specifier: "lodash-es",
            packageName: "lodash-es",
            named: [],
            importKind: "default",
          }),
          shape,
          1_000,
        ),
        undefined,
        `a \`${stage}\` result must not even be CONSTRUCTIBLE as a history row`,
      );
    }

    assert.equal(
      bundleImpactHistoryItemForResponse(
        fileSize({ diagnostics: [{ stage, message: "combined build", details: [] }] }),
        "C:/app/index.ts",
        5,
      ),
      undefined,
      `a file total degraded by \`${stage}\` is not this file's size`,
    );
    assert.equal(
      isDurableFileSize(fileSize({ diagnostics: [{ stage, message: "x", details: [] }] })),
      false,
    );
  }
});

test("a measurement is durable and a deterministic failure has nothing to record", () => {
  assert.equal(isDurableImportResult(measured), true);
  assert.equal(
    isDurableImportResult(failedToParse),
    false,
    "a parse failure is a fact about the code, and the daemon caches it — but it has no SIZE, and a history row is five sizes",
  );
  assert.equal(isDurableImportResult(undefined), false);
});

test("the persisted import-cost history records only what was measured", () => {
  const items = importCostHistoryItemsForStates(
    [stateFor("lodash-es", failedToParse), stateFor("dayjs", measured)],
    1_000,
  );

  assert.deepEqual(
    items.map((item) => item.specifier),
    ["dayjs"],
  );
});

test("the persisted bundle-impact history refuses a floor and takes a measurement", () => {
  assert.equal(
    bundleImpactHistoryItemForResponse(fileSize({ incomplete: true }), "C:/app/index.ts", 5),
    undefined,
    "recording a floor makes the next honest sizing of this file read as a regression",
  );
  assert.deepEqual(
    bundleImpactHistoryItemForResponse(
      fileSize({ states: [itemFor("dayjs"), itemFor("clsx")] }),
      "C:/app/index.ts",
      5,
    ),
    {
      timestamp: 5,
      fileName: "C:/app/index.ts",
      rawBytes: 1_000,
      minifiedBytes: 500,
      gzipBytes: 200,
      brotliBytes: 180,
      zstdBytes: 190,
      // From `states`, not `imports` — instance #5. `imports` holds only the results the daemon
      // HAD when it answered; on a streamed read the ones still building are absent, so this
      // recorded a file's two imports as zero. `incomplete` cannot catch it: it guards the bytes.
      importCount: 2,
    },
  );
});

test("an incomplete or errored file total is not a durable data point", () => {
  assert.equal(isDurableFileSize(fileSize()), true);
  assert.equal(
    isDurableFileSize(fileSize({ incomplete: true })),
    false,
    "a floor is a real number, but it is not this file's size",
  );
  assert.equal(isDurableFileSize(fileSize({ error: "document parse failed" })), false);
  assert.equal(
    isDurableFileSize(
      fileSize({
        diagnostics: [{ stage: "entry_resolution", message: "no entry", details: [] }],
      }),
    ),
    true,
    "a deterministic diagnostic does not make the totals a floor",
  );
});
