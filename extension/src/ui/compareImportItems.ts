import type { ImportResult } from "../ipc/protocol.js";
import { formatBytes } from "./format.js";

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

  const items = results
    .filter((result) => !result.error)
    .sort((left, right) => left.brotli_bytes - right.brotli_bytes)
    .map((result) => ({
      label: `${result.specifier}: ${formatBytes(result.brotli_bytes)} br`,
      detail: `${formatBytes(result.minified_bytes)} min · ${formatBytes(result.gzip_bytes)} gz · ${formatBytes(result.zstd_bytes)} zstd`,
    }));

  if (items.length === 0) {
    return {
      items,
      warning: "Import Lens could not compute any comparison results.",
    };
  }

  return { items };
};
