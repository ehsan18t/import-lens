import type { ImportResult } from "../ipc/protocol.js";
import { formatBytes, type MeasuredSizes, measuredSizes } from "./format.js";

export interface CompareImportQuickPickItem {
  label: string;
  detail: string;
}

export interface CompareImportItemsResult {
  items: CompareImportQuickPickItem[];
  warning?: string;
}

export const compareImportItemsForResults = (
  results: readonly ImportResult[] | null,
): CompareImportItemsResult => {
  if (!results) {
    return {
      items: [],
      warning: "Import Lens daemon did not return comparison results.",
    };
  }

  // A comparison is an ordering of sizes, so only an import that HAS one can be in it. Filtering on
  // `!result.error` let a fabricated size into the ranking, where it sorted as the cheapest option
  // and recommended itself.
  const items = results
    .map((result): [ImportResult, MeasuredSizes | null] => [result, measuredSizes(result)])
    .filter((pair): pair is [ImportResult, MeasuredSizes] => pair[1] !== null)
    .sort(([, left], [, right]) => left.brotli_bytes - right.brotli_bytes)
    .map(([result, sizes]) => ({
      label: `${result.specifier}: ${formatBytes(sizes.brotli_bytes)} br`,
      detail: `${formatBytes(sizes.minified_bytes)} min · ${formatBytes(sizes.gzip_bytes)} gz · ${formatBytes(sizes.zstd_bytes)} zstd`,
    }));

  if (items.length === 0) {
    return {
      items,
      warning: "Import Lens could not compute any comparison results.",
    };
  }

  return { items };
};
