import type { ImportAnalysisItem } from "../ipc/protocol.js";
import { formatBytes, type MeasuredSizes, measuredSizes } from "./format.js";

export interface CompareImportQuickPickItem {
  label: string;
  detail?: string;
  /** Rendered as a `QuickPickItemKind.Separator` at the vscode edge. Carries no size. */
  separator?: true;
}

export interface CompareImportItemsResult {
  items: CompareImportQuickPickItem[];
  /** How many of the requested specifiers carry a size and are therefore in the ranking. */
  comparedCount: number;
  /** How many were requested but could not be ranked — always disclosed, never silently dropped. */
  excludedCount: number;
  /** Fatal only: there is nothing to show, so no pick is opened. */
  warning?: string;
}

/**
 * A requested specifier can fall out of a comparison in three places, and all three used to be
 * silent: the user asked to compare four packages, saw two rows, and was told nothing — in a pick
 * whose entire purpose is "which of these is cheapest".
 *
 * 1. The daemon never treats it as a package import (a relative path, a `node:` builtin, a
 *    framework-virtual specifier), so it never appears in the response at all.
 * 2. It came back with no result — not installed, or unresolvable — carrying the daemon's `message`.
 * 3. It came back with a result that has no size, carrying `error`.
 *
 * The ranking itself still admits only imports that HAVE a size: filtering on `!result.error` once
 * let a fabricated size into the ordering, where it sorted cheapest and recommended itself. What
 * changes here is that the exclusions are now shown, with the reason the daemon gave.
 */
const excludedComparisonEntries = (
  requested: readonly string[],
  analysed: readonly ImportAnalysisItem[],
): CompareImportQuickPickItem[] => {
  const analysedSpecifiers = new Set(analysed.map((item) => item.detected.specifier));
  const notAnalysed = requested
    .filter((specifier) => !analysedSpecifiers.has(specifier))
    .map((specifier) => ({
      label: specifier,
      detail: "Not compared: not a package import Import Lens can size",
    }));

  const unsized = analysed.flatMap((item) => {
    if (!item.result) {
      return [
        {
          label: item.detected.specifier,
          detail: `Not compared: ${item.message ?? "the daemon returned no result for this import"}`,
        },
      ];
    }

    if (measuredSizes(item.result)) {
      return [];
    }

    return [
      {
        label: item.detected.specifier,
        detail: `Not compared: ${item.result.error ?? "no size was produced for this import"}`,
      },
    ];
  });

  return [...notAnalysed, ...unsized];
};

const comparisonFailureWarning = (excluded: readonly CompareImportQuickPickItem[]): string => {
  if (excluded.length === 0) {
    return "Import Lens could not compute any comparison results.";
  }

  const reasons = excluded
    .slice(0, 3)
    .map((entry) => `${entry.label} (${entry.detail?.replace("Not compared: ", "") ?? "no size"})`)
    .join("; ");
  const rest = excluded.length > 3 ? `, and ${excluded.length - 3} more` : "";

  return `Import Lens could not compare any of these imports: ${reasons}${rest}.`;
};

export const compareImportItemsForResults = (
  requested: readonly string[],
  analysed: readonly ImportAnalysisItem[] | null,
): CompareImportItemsResult => {
  if (!analysed) {
    return {
      items: [],
      comparedCount: 0,
      excludedCount: requested.length,
      warning: "Import Lens daemon did not return comparison results.",
    };
  }

  const ranked = analysed
    .flatMap((item): [string, MeasuredSizes][] => {
      const sizes = item.result ? measuredSizes(item.result) : null;

      return sizes ? [[item.result?.specifier ?? item.detected.specifier, sizes]] : [];
    })
    .sort(([, left], [, right]) => left.brotli_bytes - right.brotli_bytes)
    .map(([specifier, sizes]) => ({
      label: `${specifier}: ${formatBytes(sizes.brotli_bytes)} br`,
      detail: `${formatBytes(sizes.minified_bytes)} min · ${formatBytes(sizes.gzip_bytes)} gz · ${formatBytes(sizes.zstd_bytes)} zstd`,
    }));

  const excluded = excludedComparisonEntries(requested, analysed);

  if (ranked.length === 0) {
    return {
      items: [],
      comparedCount: 0,
      excludedCount: excluded.length,
      warning: comparisonFailureWarning(excluded),
    };
  }

  const items =
    excluded.length === 0
      ? ranked
      : [
          ...ranked,
          {
            label: `${excluded.length} not compared`,
            separator: true as const,
          },
          ...excluded,
        ];

  return { items, comparedCount: ranked.length, excludedCount: excluded.length };
};
