import assert from "node:assert/strict";
import test from "node:test";
import {
  bundleImpactHistoryLabel,
  recordBundleImpactHistory,
  type BundleImpactHistoryItem,
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
    store.get<BundleImpactHistoryItem[]>("importLens.bundleImpactHistory", []).map((entry) => entry.fileName),
    ["/workspace/src/c.ts", "/workspace/src/b.ts"],
  );
});

test("bundleImpactHistoryLabel formats bundle history entries", () => {
  assert.equal(
    bundleImpactHistoryLabel(item("/workspace/src/app.ts", 1500)),
    "1.5 kB br · 5.0 kB min · 2 imports · app.ts",
  );
});
