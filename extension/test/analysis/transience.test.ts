import assert from "node:assert/strict";
import test from "node:test";
import {
  type BundleImpactHistoryItem,
  bundleImpactHistoryItemForResponse,
  bundleImpactHistoryKey,
  type ImportCostHistoryItem,
  importCostHistoryItemsForStates,
  importCostHistoryKey,
  recordBundleImpactHistory,
  recordImportCostHistory,
} from "../../src/analysis/history.js";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import {
  isDurableFileSize,
  isDurableImportResult,
  transientAnalysisStages,
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

class MemoryStore {
  readonly values = new Map<string, unknown>();

  get<T>(key: string, defaultValue: T): T {
    return (this.values.get(key) as T | undefined) ?? defaultValue;
  }

  async update(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
  }
}

/**
 * **Property** over EVERY transient stage the daemon declares × every durable store the extension
 * owns. A stage added to `transientAnalysisStages` that some store forgets to gate on fails here.
 *
 * **It feeds the STORES, not the constructors.** It used to feed `importCostHistoryItem` and
 * `bundleImpactHistoryItemForResponse` — the row builders — and assert they returned `undefined`.
 * That proved the predicate worked; it proved nothing about the store, which took a
 * `ImportCostHistoryItem[]` and wrote down whatever it was handed. A row built by hand went
 * straight into `globalState`. That is precisely the predicate-beside-a-store shape the daemon
 * fixed on its side and FR-026c says must not exist, and it was still standing here.
 *
 * The extension's stores are the worst place in the system for a fabricated number to land: they
 * have no TTL, no cache generation, and one row per identity, so the value does not go stale — it
 * becomes that import's permanent baseline, and every later trend is measured against a number
 * that never happened.
 */
test("no durable store takes a transient outcome, in any of its shapes", async () => {
  assert.ok(transientAnalysisStages.length > 0, "there must be at least one transient stage");

  for (const stage of transientAnalysisStages) {
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

    // **The import-cost STORE.** Hand it the states — a transient failure beside a healthy import —
    // and only the healthy one may be written down. There is no way to hand it a row.
    for (const shape of [unmeasured(stage), comparisonDegraded(stage)]) {
      const store = new MemoryStore();
      await recordImportCostHistory(
        store,
        [stateFor("lodash-es", shape), stateFor("dayjs", measured)],
        1_000,
      );

      assert.deepEqual(
        store.get<ImportCostHistoryItem[]>(importCostHistoryKey, []).map((item) => item.specifier),
        ["dayjs"],
        `a \`${stage}\` row would replace lodash-es's real baseline for good, and the STORE must be \
the thing that refuses it`,
      );
    }

    // **The bundle-impact STORE.** Same shape: it takes the response, applies the gate itself, and
    // writes nothing when the totals are not this file's.
    const fileStore = new MemoryStore();
    await recordBundleImpactHistory(
      fileStore,
      fileSize({ diagnostics: [{ stage, message: "combined build", details: [] }] }),
      "C:/app/index.ts",
      5,
    );
    assert.deepEqual(
      fileStore.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []),
      [],
      `a file total degraded by \`${stage}\` is not this file's size`,
    );

    assert.equal(
      isDurableFileSize(fileSize({ diagnostics: [{ stage, message: "x", details: [] }] })),
      false,
    );
  }
});

/**
 * **The shape `incomplete` cannot see** (ADR-0006, invariant 4, second half): every contributor
 * Measured, `error: null`, `incomplete: false` — and the file's own combined build failed, so the
 * number is an un-deduplicated sum of per-import costs. An over-count, and a different quantity
 * from a File Cost (ADR-0004).
 *
 * The `degraded` flag is its only evidence, and this store is one of the three consumers that has
 * to refuse it (the L1 aggregate cache and `importlens check` are the others).
 */
test("the bundle-impact store refuses a total whose own combined build failed", async () => {
  const response = fileSize({ degraded: true, incomplete: false, error: null });

  assert.equal(
    isDurableFileSize(response),
    false,
    "a per-import sum with shared modules counted twice is not this file's size",
  );

  const store = new MemoryStore();
  await recordBundleImpactHistory(store, response, "C:/app/index.ts", 5);

  assert.deepEqual(
    store.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []),
    [],
    "an over-count persisted with no TTL becomes this file's permanent baseline, and the next \
honest sizing reads as an improvement that never happened",
  );
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

test("a measured asset I/O fallback is not durable", async () => {
  const assetIoFallback = comparisonDegraded("asset_io");

  assert.equal(
    isDurableImportResult(assetIoFallback),
    false,
    "the sizes omit asset bytes because of this machine's filesystem state",
  );
  assert.equal(
    isDurableFileSize(
      fileSize({ diagnostics: [{ stage: "asset_io", message: "read failed", details: [] }] }),
    ),
    false,
    "the same undercount cannot become a persisted File Cost baseline",
  );

  const store = new MemoryStore();
  await recordImportCostHistory(
    store,
    [stateFor("asset-lib", assetIoFallback), stateFor("dayjs", measured)],
    1_000,
  );
  assert.deepEqual(
    store.get<ImportCostHistoryItem[]>(importCostHistoryKey, []).map((item) => item.specifier),
    ["dayjs"],
  );
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
      // A sound measurement, so the number is a size rather than a disclosed upper bound. The flag
      // is persisted because a delta is only a change if both sides are the same kind of number.
      imprecise: false,
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
