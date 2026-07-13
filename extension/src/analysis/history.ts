import path from "node:path";
import type { DetectedImport, FileSizeDocumentResponse, ImportResult } from "../ipc/protocol.js";
import { formatBytes, measuredSizes } from "../ui/format.js";
import { isDurableFileSize, isDurableImportResult } from "./transience.js";

// The two persisted histories live in `globalState`: no TTL, no cache generation, no Clear Caches
// command behind them, and one row per identity. A row that should not be there does not go stale —
// it REPLACES that import's (or that file's) real baseline permanently, and every later trend is
// computed against a number that never happened. So both stores below take the raw daemon output and
// apply the gate themselves; neither will accept a row (ADR-0006, invariant 3; SRS FR-026c).

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
    // `states`, not `imports` — instance #5 of the one defect. `imports` holds only the results
    // the daemon HAD when it answered; on a streamed read the ones still building are absent, so
    // this recorded "3 imports" as "1 import" and the bundle-impact chart showed a file shedding
    // two imports it never lost. The `incomplete` gate above does not catch it, because
    // `incomplete` guards the BYTES: a still-loading import makes the total a floor, but a
    // still-loading import is exactly one the count must include. `states` is the file's imports as
    // the FILE has them.
    importCount: response.states.length,
  };
};

/**
 * Write a sized document to the persisted bundle-impact history — **if it is a measurement of that
 * file**.
 *
 * The store takes the RESPONSE, not a row, and that is the whole point. It used to take a
 * `BundleImpactHistoryItem`, which is five numbers and a filename: by the time one exists, every
 * trace of how it was measured is gone, so the store could not re-derive whether it was safe to
 * keep and had to trust that its caller had asked. That is a predicate beside a store, which is the
 * exact shape of this defect — something the next caller can forget, with nothing failing when they
 * do. Handing it the response instead makes forgetting impossible: the gate is the store's own.
 *
 * The daemon fixed this same shape on its side (`FileSizeCache::insert` asks `is_cacheable` itself);
 * this is the other half of it (FR-026c).
 */
export const recordBundleImpactHistory = async (
  store: BundleImpactHistoryStore,
  response: FileSizeDocumentResponse,
  fileName: string,
  timestamp: number = Date.now(),
  limit = 20,
): Promise<BundleImpactHistoryItem | undefined> => {
  const item = bundleImpactHistoryItemForResponse(response, fileName, timestamp);

  if (!item) {
    return undefined;
  }

  const existing = store.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []);
  await store.update(bundleImpactHistoryKey, [item, ...existing].slice(0, Math.max(1, limit)));
  return item;
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

/**
 * The history row an import result contributes — or `undefined` when that result may not be written
 * down.
 *
 * **The gate is here, in the only constructor of the row.** `ImportCostHistoryItem` is five sizes
 * and an identity: once it exists, every trace of *how* it was measured is gone, so
 * `recordImportCostHistory` cannot re-derive whether it was safe to keep. Gating at the caller
 * instead left the store takeable — build a row by hand and it goes into `globalState`, which has no
 * TTL, no cache generation, and one row per identity, so a bad row does not go stale: it replaces
 * that import's real baseline permanently. Making the row unconstructible from a result the daemon
 * would not itself cache is what closes that (ADR-0006, invariant 3).
 *
 * Two refusals, and `isDurableImportResult` is both. A result with **no size** has nothing to record
 * — returning `undefined` rather than defaulting to zero is what stops a "was 17 kB, now 0 B" trend
 * against an import the engine merely could not measure this time. And a result whose measurement a
 * **transient** failure degraded describes this moment's scheduling, not the package.
 */
export const importCostHistoryItem = (
  detected: DetectedImport,
  result: ImportResult,
  timestamp: number = Date.now(),
): ImportCostHistoryItem | undefined => {
  if (!isDurableImportResult(result)) {
    return undefined;
  }

  const sizes = measuredSizes(result);

  if (!sizes) {
    return undefined;
  }

  return {
    identity: importCostHistoryIdentity(detected),
    timestamp,
    specifier: detected.specifier,
    importKind: detected.importKind,
    named: [...detected.named],
    rawBytes: sizes.raw_bytes,
    minifiedBytes: sizes.minified_bytes,
    gzipBytes: sizes.gzip_bytes,
    brotliBytes: sizes.brotli_bytes,
    zstdBytes: sizes.zstd_bytes,
  };
};

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

/**
 * The rows a document's analysis states contribute to the persisted import-cost history.
 *
 * A `filter`, not a `map`: `importCostHistoryItem` refuses a result that may not be written down,
 * and the refusals simply do not become rows.
 */
export const importCostHistoryItemsForStates = (
  states: readonly ImportCostHistorySource[],
  now: number = Date.now(),
): ImportCostHistoryItem[] =>
  states
    .filter((state) => state.status === "ready" && state.result !== undefined)
    .map((state) => importCostHistoryItem(state.detected, state.result as ImportResult, now))
    .filter((item): item is ImportCostHistoryItem => item !== undefined);

/**
 * What the import-cost store takes: an import's identity and the result the daemon gave for it.
 *
 * Structural, so `ImportAnalysisState` satisfies it without this module having to import the UI's
 * state type (and without the cycle that would create).
 */
export interface ImportCostHistorySource {
  detected: DetectedImport;
  status: string;
  result?: ImportResult;
}

let historyWriteChain: Promise<void> = Promise.resolve();

/**
 * Write a document's imports to the persisted import-cost history — **each one only if it may be
 * written down**.
 *
 * Like `recordBundleImpactHistory`, this takes the analysis states and builds the rows itself. It
 * used to take `readonly ImportCostHistoryItem[]`, so the gate lived in the row's constructor and
 * the store would accept any row anyone handed it. That is not a store with a gate; it is a store
 * beside a predicate, and FR-026c says the difference is the entire lesson.
 */
export const recordImportCostHistory = (
  store: BundleImpactHistoryStore,
  states: readonly ImportCostHistorySource[],
  now: number = Date.now(),
  limit = 200,
): Promise<void> => {
  const items = importCostHistoryItemsForStates(states, now);

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
