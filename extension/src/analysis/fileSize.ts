import type { FileSizeDocumentResponse, ImportAnalysisItem } from "../ipc/protocol.js";
import {
  bytesForCompression,
  type CompressionFormat,
  formatBytes,
  labelForCompression,
} from "../ui/format.js";
import {
  fileCostBecause,
  fileCostQuality,
  fileCostQuantityName,
  isFileCost,
} from "./fileCostQuality.js";
import { type BundleImpactHistoryItem, bundleImpactHistoryDeltaLabel } from "./history.js";

/**
 * The imports of the file, as the file has them: every runtime package import the daemon DETECTED.
 *
 * `states`, not `imports`. An interactive size read is streamed — the daemon answers a cold import
 * `loading` and puts only the ones it has already MEASURED in `imports` — so on a document nobody
 * has sized yet that list is empty while the file's own totals, which come from the combined build
 * rather than from the per-import measurements, are perfectly real. Counting `imports` reports "0
 * imports" for a file that has three, and gating on it reports "no resolvable package imports" for
 * a file the daemon sized fine. `listener.ts` carries the same warning for the status bar.
 */
const importCountFor = (response: Pick<FileSizeDocumentResponse, "states">): number =>
  response.states.length;

/**
 * The imports the daemon could not size, and never will on this response: no manifest, no
 * resolution, a build that could not answer. An import still being measured is NOT one of them — it
 * is why the total is an estimate, which the summary says separately.
 */
const skippedCountFor = (response: Pick<FileSizeDocumentResponse, "states">): number =>
  response.states.filter((state) => isUnsizable(state.status)).length;

const isUnsizable = (status: ImportAnalysisItem["status"]): boolean =>
  status === "missing" || status === "unavailable";

/**
 * The summary line, headed by **the name of the quantity it is showing**.
 *
 * It said `Current file: 183.2 kB br`, which names no quantity at all — on a file whose real File
 * Cost was 118.0 kB and whose combined build had failed, leaving an un-deduplicated sum of its five
 * imports. "Current file" is where the number was measured, not what it is.
 */
export const formatCurrentFileSizeSummary = (
  response: FileSizeDocumentResponse,
  compression: CompressionFormat,
): string => {
  const importCount = importCountFor(response);
  const importLabel = importCount === 1 ? "import" : "imports";

  return [
    `${fileCostQuantityName(fileCostQuality(response))}: ${formatBytes(bytesForCompression(response, compression))} ${labelForCompression(compression)}`,
    `${formatBytes(response.minified_bytes)} min`,
    `${importCount} ${importLabel}`,
  ].join(" · ");
};

/**
 * The **File Cost** of one document: ONE bundle over all its imports, so a module two of them reach
 * is counted **once** (ADR-0004). It is the file's size, and it is the ONLY number the per-file
 * budget may be judged against.
 *
 * It comes off `file_size_document`, which is why it has to be carried: the budget check runs over
 * the analysis store's per-import states, and summing THOSE gives a *Combined Import Cost* — an
 * upper bound that counts a shared graph once per import, which warned a file with five
 * `@mui/material` subpath imports as 3x over budget while the status bar, one line away, showed it
 * inside budget.
 *
 * It carries the daemon's honesty flags with it, unread: whether the number may be judged at all is
 * {@link isDurableFileSize}'s question and nobody else's, and asking it a second way here is how the
 * same defect got into three consumers (FR-026c, and the drift check that now holds them together).
 */
export interface DocumentFileCost
  extends Pick<FileSizeDocumentResponse, "error" | "diagnostics" | "incomplete" | "degraded"> {
  brotliBytes: number;
}

export const documentFileCost = (response: FileSizeDocumentResponse): DocumentFileCost => ({
  brotliBytes: response.brotli_bytes,
  error: response.error,
  diagnostics: response.diagnostics,
  incomplete: response.incomplete,
  degraded: response.degraded,
});

/** What the "Show current file size" command can say about a size response. */
export type CurrentFileSizeReport = { kind: "no-imports" } | { kind: "summary"; message: string };

export interface CurrentFileSizeHistory {
  /**
   * The bundle-impact row this response contributes, or `undefined` when its totals are a FLOOR
   * rather than the file's size (an import or asset contribution is missing, or the combined build
   * degraded). A floor is worth SHOWING — it beats a blank — and must never be recorded or
   * compared: the history has no TTL and keeps one row per file, so it would become that file's
   * baseline and make the next honest sizing read as a regression.
   */
  current?: BundleImpactHistoryItem;
  /** The file's previous MEASURED total, if it has one. */
  previous?: BundleImpactHistoryItem;
}

/**
 * The one decision the command makes about a size response, kept vscode-free so it is testable: a
 * file with no runtime package imports has nothing to report, and every other file has a number —
 * including the cold one, whose per-import builds have not landed yet.
 */
export const currentFileSizeReport = (
  response: FileSizeDocumentResponse,
  compression: CompressionFormat,
  history: CurrentFileSizeHistory = {},
): CurrentFileSizeReport => {
  if (importCountFor(response) === 0) {
    return { kind: "no-imports" };
  }

  const skipped = skippedCountFor(response);
  const skippedSuffix = skipped > 0 ? ` · ${skipped} skipped` : "";
  // No delta against a floor: the comparison would be arithmetic on a number the file never had,
  // and would report a regression or a win that did not happen.
  const diffSuffix =
    history.previous && history.current
      ? ` · ${bundleImpactHistoryDeltaLabel(history.current, history.previous)}`
      : "";
  // WHY the number is what it is, read off the response — not inferred from whether the history
  // store kept it. The suffix used to be keyed on `history.current` being absent, and the store
  // withholds that for a floor and for an un-deduplicated sum alike, so a `degraded` response
  // borrowed `incomplete`'s explanation: "estimate (some imports are not fully measured)" on a file
  // where every single import IS fully measured, and the file's own build is what failed.
  const quality = fileCostQuality(response);
  const becauseSuffix = isFileCost(quality) ? "" : ` · ${fileCostBecause(quality)}`;

  return {
    kind: "summary",
    message: `${formatCurrentFileSizeSummary(response, compression)}${skippedSuffix}${diffSuffix}${becauseSuffix}`,
  };
};
