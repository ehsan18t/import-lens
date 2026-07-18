import assert from "node:assert/strict";
import test from "node:test";
import {
  type BundleImpactHistoryItem,
  bundleImpactHistoryDeltaLabel,
  bundleImpactHistoryKey,
  bundleImpactHistoryLabel,
  type ImportCostHistoryItem,
  type ImportCostHistorySource,
  importCostHistoryDeltaLabel,
  importCostHistoryKey,
  recordBundleImpactHistory,
  recordImportCostHistory,
} from "../../src/analysis/history.js";
import type { FileSizeDocumentResponse, ImportResult } from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

class MemoryStore {
  readonly values = new Map<string, unknown>();

  get<T>(key: string, defaultValue: T): T {
    return (this.values.get(key) as T | undefined) ?? defaultValue;
  }

  async update(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
  }
}

const item = (fileName: string, brotliBytes: number): BundleImpactHistoryItem => ({
  timestamp: 1_800_000,
  fileName,
  rawBytes: 10_000,
  minifiedBytes: 5_000,
  gzipBytes: 1_900,
  brotliBytes,
  zstdBytes: 1_700,
  importCount: 2,
  imprecise: false,
});

/** A measured document: the store takes the RESPONSE and builds the row itself. */
const sizedDocument = (brotliBytes: number): FileSizeDocumentResponse => ({
  version: 7,
  request_id: 1,
  raw_bytes: 10_000,
  minified_bytes: 5_000,
  gzip_bytes: 1_900,
  brotli_bytes: brotliBytes,
  zstd_bytes: 1_700,
  imports: [],
  states: [
    { detected: detectedImport({ specifier: "dayjs", packageName: "dayjs" }), status: "ready" },
    { detected: detectedImport({ specifier: "clsx", packageName: "clsx" }), status: "ready" },
  ],
  error: null,
  diagnostics: [],
});

test("recordBundleImpactHistory keeps newest entries first under the limit", async () => {
  const store = new MemoryStore();

  await recordBundleImpactHistory(store, sizedDocument(1500), "/workspace/src/a.ts", 1_800_000, 2);
  await recordBundleImpactHistory(store, sizedDocument(1200), "/workspace/src/b.ts", 1_800_000, 2);
  await recordBundleImpactHistory(store, sizedDocument(900), "/workspace/src/c.ts", 1_800_000, 2);

  assert.deepEqual(
    store.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []).map((entry) => entry.fileName),
    ["/workspace/src/c.ts", "/workspace/src/b.ts"],
  );
});

test("bundle-impact history leaves rows without quality metadata behind", async () => {
  const store = new MemoryStore();
  // v3 rows carry no `imprecise` flag, so a disclosed upper bound recorded under them cannot be
  // told from a real baseline — the same reason v2 was left behind for pre-D12 asset floors.
  const legacyKey = "importLens.bundleImpactHistory.v3";
  store.values.set(legacyKey, [item("/workspace/src/legacy-floor.ts", 100)]);

  await recordBundleImpactHistory(
    store,
    sizedDocument(1500),
    "/workspace/src/complete.ts",
    1_800_001,
  );

  assert.equal(bundleImpactHistoryKey, "importLens.bundleImpactHistory.v4");
  assert.deepEqual(
    store.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []).map((entry) => entry.fileName),
    ["/workspace/src/complete.ts"],
  );
  assert.deepEqual(
    store.get<BundleImpactHistoryItem[]>(legacyKey, []).map((entry) => entry.fileName),
    ["/workspace/src/legacy-floor.ts"],
    "old rows have no quality metadata, so they are ignored rather than migrated as real baselines",
  );
});

test("bundleImpactHistoryLabel formats bundle history entries", () => {
  assert.equal(
    bundleImpactHistoryLabel(item("/workspace/src/app.ts", 1500)),
    "1.5 kB br · 5.0 kB min · 2 imports · app.ts",
  );
});

test("bundleImpactHistoryDeltaLabel formats import cost deltas", () => {
  assert.equal(
    bundleImpactHistoryDeltaLabel(
      item("/workspace/src/app.ts", 1800),
      item("/workspace/src/app.ts", 1500),
    ),
    "+300 B br vs previous",
  );
  assert.equal(
    bundleImpactHistoryDeltaLabel(
      item("/workspace/src/app.ts", 1200),
      item("/workspace/src/app.ts", 1500),
    ),
    "-300 B br vs previous",
  );
});

/**
 * A measured import, as an analysis state — the only thing `recordImportCostHistory` accepts.
 *
 * It used to accept a finished `ImportCostHistoryItem`, so this helper built one directly and the
 * store wrote it down. That is what "the gate is in the constructor, not the store" bought: a row
 * nobody gated went into `globalState`, which has no TTL. The store takes the raw result now, so
 * there is no way to express the bad call.
 */
const costSource = (specifier: string, brotliBytes: number): ImportCostHistorySource => ({
  detected: detectedImport({ specifier, packageName: specifier, importKind: "named", named: [] }),
  status: "ready",
  result: {
    specifier,
    raw_bytes: brotliBytes * 4,
    minified_bytes: brotliBytes * 2,
    gzip_bytes: brotliBytes,
    brotli_bytes: brotliBytes,
    zstd_bytes: brotliBytes,
    cache_hit: false,
    side_effects: false,
    truly_treeshakeable: true,
    is_cjs: false,
    confidence: "high",
    confidence_reasons: [],
    error: null,
    diagnostics: [],
  } satisfies ImportResult,
});

class SlowStore extends MemoryStore {
  override async update(key: string, value: unknown): Promise<void> {
    await Promise.resolve();
    this.values.set(key, value);
  }
}

test("concurrent import-cost history writes both persist", async () => {
  const store = new SlowStore();

  await Promise.all([
    recordImportCostHistory(store, [costSource("react", 100)]),
    recordImportCostHistory(store, [costSource("lodash-es", 200)]),
  ]);

  const specifiers = store
    .get<ImportCostHistoryItem[]>(importCostHistoryKey, [])
    .map((entry) => entry.specifier);
  assert.ok(specifiers.includes("react"), `react missing: ${specifiers.join(",")}`);
  assert.ok(specifiers.includes("lodash-es"), `lodash-es missing: ${specifiers.join(",")}`);
});

test("recording a changed import cost keeps one row per identity", async () => {
  const store = new MemoryStore();

  await recordImportCostHistory(store, [costSource("react", 100)]);
  await recordImportCostHistory(store, [costSource("react", 150)]);

  const rows = store
    .get<ImportCostHistoryItem[]>(importCostHistoryKey, [])
    .filter((entry) => entry.specifier === "react");
  assert.equal(rows.length, 1);
  assert.equal(rows[0].brotliBytes, 150);
});

/**
 * A delta is only a change if both sides are the same kind of number.
 *
 * An `imprecise_assets` total is a disclosed UPPER BOUND, and FR-032a says it may enter history —
 * it is deterministic and reusable, so refusing the row would be the wrong fix. What was wrong is
 * that the row kept no record of being one, which made re-validation not merely absent but
 * structurally impossible. The direction that mattered was the silent one: an imprecise CURRENT
 * result was captioned, while an imprecise BASELINE with a sound run against it rendered a clean
 * byte-exact saving that was partly the over-count evaporating.
 */
test("a delta measured against an upper-bound baseline says so", () => {
  const baseline: BundleImpactHistoryItem = {
    ...item("/workspace/src/a.ts", 65_000),
    imprecise: true,
  };
  const sound = item("/workspace/src/a.ts", 40_000);

  assert.ok(
    bundleImpactHistoryDeltaLabel(sound, baseline).endsWith(", against an upper bound"),
    "a sound run measured against a stored upper bound is not a like-for-like comparison",
  );
  assert.ok(
    !bundleImpactHistoryDeltaLabel(sound, item("/workspace/src/a.ts", 45_000)).includes(
      "upper bound",
    ),
    "two sound rows compare cleanly and must not be caveated",
  );
});

/** The import axis carried no caveat in EITHER direction, so it was the worse of the two. */
test("an import delta measured against an upper-bound baseline says so", () => {
  const importItem = (brotliBytes: number, imprecise = false): ImportCostHistoryItem => ({
    identity: "react|named|",
    timestamp: 1_800_000,
    specifier: "react",
    importKind: "named",
    named: [],
    rawBytes: brotliBytes * 4,
    minifiedBytes: brotliBytes * 2,
    gzipBytes: brotliBytes,
    brotliBytes,
    zstdBytes: brotliBytes,
    imprecise,
  });
  const previous = importItem(9_000, true);
  const current = importItem(4_000);

  assert.ok(
    importCostHistoryDeltaLabel(current, previous).endsWith(", against an upper bound"),
    "the import axis must caveat an upper-bound baseline too",
  );
  assert.ok(
    !importCostHistoryDeltaLabel(current, importItem(5_000)).includes("upper bound"),
    "two sound rows compare cleanly",
  );
});
