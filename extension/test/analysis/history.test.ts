import assert from "node:assert/strict";
import test from "node:test";
import {
  type BundleImpactHistoryItem,
  bundleImpactHistoryDeltaLabel,
  bundleImpactHistoryLabel,
  type ImportCostHistoryItem,
  importCostHistoryKey,
  recordBundleImpactHistory,
  recordImportCostHistory,
} from "../../src/analysis/history.js";

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
});

test("recordBundleImpactHistory keeps newest entries first under the limit", async () => {
  const store = new MemoryStore();

  await recordBundleImpactHistory(store, item("/workspace/src/a.ts", 1500), 2);
  await recordBundleImpactHistory(store, item("/workspace/src/b.ts", 1200), 2);
  await recordBundleImpactHistory(store, item("/workspace/src/c.ts", 900), 2);

  assert.deepEqual(
    store
      .get<BundleImpactHistoryItem[]>("importLens.bundleImpactHistory", [])
      .map((entry) => entry.fileName),
    ["/workspace/src/c.ts", "/workspace/src/b.ts"],
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

const costItem = (identity: string, brotliBytes: number): ImportCostHistoryItem => ({
  identity,
  timestamp: 1_800_000,
  specifier: identity,
  importKind: "named",
  named: [],
  rawBytes: brotliBytes * 4,
  minifiedBytes: brotliBytes * 2,
  gzipBytes: brotliBytes,
  brotliBytes,
  zstdBytes: brotliBytes,
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
    recordImportCostHistory(store, [costItem("react", 100)]),
    recordImportCostHistory(store, [costItem("lodash-es", 200)]),
  ]);

  const identities = store
    .get<ImportCostHistoryItem[]>(importCostHistoryKey, [])
    .map((entry) => entry.identity);
  assert.ok(identities.includes("react"), `react missing: ${identities.join(",")}`);
  assert.ok(identities.includes("lodash-es"), `lodash-es missing: ${identities.join(",")}`);
});

test("recording a changed import cost keeps one row per identity", async () => {
  const store = new MemoryStore();

  await recordImportCostHistory(store, [costItem("react", 100)]);
  await recordImportCostHistory(store, [costItem("react", 150)]);

  const rows = store
    .get<ImportCostHistoryItem[]>(importCostHistoryKey, [])
    .filter((entry) => entry.identity === "react");
  assert.equal(rows.length, 1);
  assert.equal(rows[0].brotliBytes, 150);
});
