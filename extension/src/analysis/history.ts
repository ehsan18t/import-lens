import path from "node:path";
import type { DetectedImport, FileSizeDocumentResponse, ImportResult } from "../ipc/protocol.js";
import { formatBytes } from "../ui/format.js";
import { isDurableFileSize } from "./transience.js";

export const bundleImpactHistoryKey = "importLens.bundleImpactHistory";
export const importCostHistoryKey = "importLens.importCostHistory";

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

export interface ImportCostHistoryItem {
  identity: string;
  timestamp: number;
  specifier: string;
  importKind: string;
  named: string[];
  rawBytes: number;
  minifiedBytes: number;
  gzipBytes: number;
  brotliBytes: number;
  zstdBytes: number;
}

/**
 * The bundle-impact row a sized document contributes to the PERSISTED history — or `undefined` when
 * its totals are not a measurement of the file.
 *
 * The gate lives here, next to the only constructor of the row, because the response gives three
 * different ways to be wrong and only one of them looks wrong: `error` is the obvious one, but a
 * combined build that timed out or panicked degrades to a conservative sum with `error: null`, and
 * an `incomplete` total is a floor whose missing input was simply an import still being measured.
 * Recording either one writes a number the file never had into a store with no TTL, and the very
 * next honest sizing then reads as a regression against it.
 */
export const bundleImpactHistoryItemForResponse = (
  response: FileSizeDocumentResponse,
  fileName: string,
  timestamp: number = Date.now(),
): BundleImpactHistoryItem | undefined => {
  if (!isDurableFileSize(response)) {
    return undefined;
  }

  return {
    timestamp,
    fileName,
    rawBytes: response.raw_bytes,
    minifiedBytes: response.minified_bytes,
    gzipBytes: response.gzip_bytes,
    brotliBytes: response.brotli_bytes,
    zstdBytes: response.zstd_bytes,
    importCount: response.imports.length,
  };
};

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

export const bundleImpactHistoryDeltaLabel = (
  current: BundleImpactHistoryItem,
  previous: BundleImpactHistoryItem,
): string => {
  const delta = current.brotliBytes - previous.brotliBytes;
  const sign = delta >= 0 ? "+" : "-";
  return `${sign}${formatBytes(Math.abs(delta))} br vs previous`;
};

export const previousBundleImpactForFile = (
  history: readonly BundleImpactHistoryItem[],
  fileName: string,
): BundleImpactHistoryItem | undefined => history.find((item) => item.fileName === fileName);

export const importCostHistoryIdentity = (detected: DetectedImport): string =>
  [detected.specifier, detected.importKind, detected.runtime, detected.named.join(",")].join("\0");

export const importCostHistoryItem = (
  detected: DetectedImport,
  result: ImportResult,
  timestamp: number = Date.now(),
): ImportCostHistoryItem => ({
  identity: importCostHistoryIdentity(detected),
  timestamp,
  specifier: detected.specifier,
  importKind: detected.importKind,
  named: [...detected.named],
  rawBytes: result.raw_bytes,
  minifiedBytes: result.minified_bytes,
  gzipBytes: result.gzip_bytes,
  brotliBytes: result.brotli_bytes,
  zstdBytes: result.zstd_bytes,
});

export const previousImportCostFor = (
  history: readonly ImportCostHistoryItem[],
  detected: DetectedImport,
): ImportCostHistoryItem | undefined =>
  history.find((item) => item.identity === importCostHistoryIdentity(detected));

export const importCostHistoryDeltaLabel = (
  current: ImportCostHistoryItem,
  previous: ImportCostHistoryItem,
): string => {
  const delta = current.brotliBytes - previous.brotliBytes;
  const sign = delta >= 0 ? "+" : "-";
  return `${sign}${formatBytes(Math.abs(delta))}`;
};

let historyWriteChain: Promise<void> = Promise.resolve();

export const recordImportCostHistory = (
  store: BundleImpactHistoryStore,
  items: readonly ImportCostHistoryItem[],
  limit = 200,
): Promise<void> => {
  // Serialize writes so concurrent analyses (e.g. switching tabs while a
  // previous file's analysis is still in flight) do not read-modify-write the
  // same array and lose each other's entries.
  const write = historyWriteChain.then(async () => {
    const existing = store.get<ImportCostHistoryItem[]>(importCostHistoryKey, []);
    const changedItems = items.filter((item) => {
      const previous = existing.find((entry) => entry.identity === item.identity);
      return !previous || !sameImportCost(item, previous);
    });

    if (changedItems.length === 0) {
      return;
    }

    // Keep one row per identity: drop prior rows for the changed identities so a
    // single frequently-edited import cannot fill the cap and evict every other
    // import's history. previousImportCostFor reads newest-first, so the trend
    // insight is unaffected.
    const changedIdentities = new Set(changedItems.map((item) => item.identity));
    const retained = existing.filter((entry) => !changedIdentities.has(entry.identity));
    await store.update(
      importCostHistoryKey,
      [...changedItems, ...retained].slice(0, Math.max(1, limit)),
    );
  });

  // Retained serialization handle only: swallow here so a failed write cannot
  // become an unhandled rejection or block the next chained write. The real
  // error still surfaces through the returned promise below.
  historyWriteChain = write.catch(() => {
    // intentionally ignored on the retained chain reference
  });
  return write;
};

const sameImportCost = (left: ImportCostHistoryItem, right: ImportCostHistoryItem): boolean =>
  left.rawBytes === right.rawBytes &&
  left.minifiedBytes === right.minifiedBytes &&
  left.gzipBytes === right.gzipBytes &&
  left.brotliBytes === right.brotliBytes &&
  left.zstdBytes === right.zstdBytes;
