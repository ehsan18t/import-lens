import path from "node:path";
import { formatBytes } from "../ui/format.js";

export const bundleImpactHistoryKey = "importLens.bundleImpactHistory";

export interface BundleImpactHistoryItem {
  timestamp: number;
  fileName: string;
  rawBytes: number;
  minifiedBytes: number;
  gzipBytes: number;
  brotliBytes: number;
  zstdBytes: number;
  importCount: number;
}

export interface BundleImpactHistoryStore {
  get<T>(key: string, defaultValue: T): T;
  update(key: string, value: unknown): Thenable<void> | Promise<void>;
}

export const recordBundleImpactHistory = async (
  store: BundleImpactHistoryStore,
  item: BundleImpactHistoryItem,
  limit = 20,
): Promise<void> => {
  const existing = store.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []);
  await store.update(bundleImpactHistoryKey, [item, ...existing].slice(0, Math.max(1, limit)));
};

export const bundleImpactHistoryLabel = (item: BundleImpactHistoryItem): string =>
  [
    `${formatBytes(item.brotliBytes)} br`,
    `${formatBytes(item.minifiedBytes)} min`,
    `${item.importCount} ${item.importCount === 1 ? "import" : "imports"}`,
    path.basename(item.fileName),
  ].join(" · ");
