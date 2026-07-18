import type { ImportAnalysisInsight } from "../analysis/state.js";
import type { ImportLensConfig } from "../config.js";
import type { AssetKind, ImportResult, ImportRuntime } from "../ipc/protocol.js";
import { confidenceVisualFor } from "./confidenceVisuals.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import {
  assetKindLabel,
  bytesForCompression,
  type CompressionFormat,
  formatBytes,
  labelForCompression,
  type MeasuredSizes,
  measuredSizes,
} from "./format.js";
import {
  isNativeBinaryOnlyResult,
  isNativeBinaryResult,
  isTypesOnlyResult,
} from "./resultDiagnostics.js";

// "This size may include unused exports" is a caveat ABOUT a size, so it needs one.
export const isConservativeEstimate = (result: ImportResult): boolean =>
  measuredSizes(result) !== null && (result.side_effects || !result.truly_treeshakeable);

export const conservativeSizingMarkdown = (result: ImportResult): string | null =>
  isConservativeEstimate(result)
    ? "$(warning) **Conservative sizing:** Size may include unused exports or side-effect modules."
    : null;

const compressionTitles: Record<Exclude<CompressionFormat, "all">, string> = {
  brotli: "Brotli",
  gzip: "Gzip",
  zstd: "Zstd",
};

const selectedCompressionSize = (
  sizes: MeasuredSizes,
  compression: CompressionFormat,
): { label: string; value: string } => ({
  label: compression === "all" ? "Brotli" : compressionTitles[compression],
  value: `${formatBytes(bytesForCompression(sizes, compression))} ${labelForCompression(compression)}`,
});

export const copyDiagnosticsMarkdown = (result: ImportResult): string => {
  const args = encodeURIComponent(JSON.stringify([result]));
  return `[$(copy) Copy diagnostics](command:${copyImportDiagnosticsCommand}?${args})`;
};

export const resultHasDiagnosticsLink = (result: ImportResult): boolean =>
  Boolean(result.error) || result.diagnostics.length > 0;

/**
 * The hover's size block — or the honest absence of one.
 *
 * It was ungated: an import with no size rendered five rows of **"NaN kB"** under a bold "Size"
 * heading. The size block is the one thing a hover cannot fake.
 */
export const importResultSizeMarkdown = (
  result: ImportResult,
  compression: CompressionFormat,
): string => {
  const sizes = measuredSizes(result);

  if (!sizes) {
    return ["**Size**", "- Size unavailable: this import could not be measured."].join("\n");
  }

  const selected = selectedCompressionSize(sizes, compression);

  return [
    "**Size**",
    `- Selected ${selected.label}: **${selected.value}**`,
    `- Raw: ${formatBytes(sizes.raw_bytes)}`,
    `- Minified: ${formatBytes(sizes.minified_bytes)}`,
    `- Gzip: ${formatBytes(sizes.gzip_bytes)}`,
    `- Brotli: ${formatBytes(sizes.brotli_bytes)}`,
    `- Zstd: ${formatBytes(sizes.zstd_bytes)}`,
    ...assetBreakdownRows(result, compression, selected.label),
  ].join("\n");
};

/**
 * How the size above is composed, when part of it is not JavaScript (B2).
 *
 * These bytes are already inside the number — a UI kit's cost is part JS and part stylesheet — so
 * this names the parts rather than adding to the total. Nothing is rendered for the common case of
 * an import that ships no assets.
 *
 * The heading names the compression these rows are in. Five differently-compressed figures are
 * listed directly above them, so an unlabelled `- CSS: 12.3 kB` invites being read as the raw
 * number when it is the selected one.
 */
const assetBreakdownRows = (
  result: ImportResult,
  compression: CompressionFormat,
  compressionLabel: string,
): string[] => {
  const breakdown = result.asset_breakdown ?? [];

  if (breakdown.length === 0) {
    return [];
  }

  return [
    "",
    `**Included assets** (${compressionLabel})`,
    ...breakdown.map(
      (contribution) =>
        `- ${assetKindLabel(contribution.kind)}: ${formatBytes(
          bytesForCompression(contribution, compression),
        )}`,
    ),
  ];
};

const yesNo = (value: boolean): "yes" | "no" => (value ? "yes" : "no");

const confidenceNotesMarkdown = (reasons: readonly string[]): string | null =>
  reasons.length > 0
    ? ["**Confidence notes**", ...reasons.map((reason) => `- ${reason}`)].join("\n")
    : null;

const analysisMarkdown = (
  result: ImportResult,
  runtime: ImportRuntime,
  confidenceBadge: string,
): string => {
  const rows = [
    "**Analysis**",
    `- Runtime: ${runtime}`,
    `- Confidence: **${confidenceBadge}**`,
    `- Side effects: ${yesNo(result.side_effects)}`,
    `- CommonJS: ${yesNo(result.is_cjs)}`,
    `- Tree-shakeable: ${yesNo(result.truly_treeshakeable)}`,
  ];

  if (result.shared_bytes && result.shared_bytes > 0) {
    rows.push(`- Shared in file: ${formatBytes(result.shared_bytes)}`);
  }

  if (isTypesOnlyResult(result)) {
    rows.push("- Type-only package: yes");
  }

  if (isNativeBinaryOnlyResult(result)) {
    rows.push("- Native binary only: yes (no importable JavaScript entry)");
  } else if (isNativeBinaryResult(result)) {
    rows.push("- Native binary: yes (the measured size is the JavaScript entry only)");
  }

  return rows.join("\n");
};

const errorDiagnosticsMarkdown = (result: ImportResult, confidenceBadge: string): string => {
  const rows = [
    "**Diagnostics**",
    "Import Lens could not compute this import size.",
    // A result with no size normally carries the reason; a still-building one carries none, and the
    // tooltip says what it knows rather than printing `null`.
    `- Error: ${result.error ?? "no size was produced for this import"}`,
    `- Confidence: **${confidenceBadge}**`,
    ...result.confidence_reasons.map((reason) => `- ${reason}`),
    `- ${copyDiagnosticsMarkdown(result)}`,
  ];

  return rows.join("\n");
};

export const tooltipForResultMarkdown = (
  result: ImportResult,
  config: Pick<ImportLensConfig, "compression">,
  runtime: ImportRuntime = "component",
  insights: readonly ImportAnalysisInsight[] = [],
): string => {
  const parts: string[] = [`**${result.specifier}**`];
  const confidence = confidenceVisualFor(result.confidence);

  // "Is there a size?", never "is there an error?" (ADR-0006, invariant 2). Everything below this
  // renders a number; the check that decides whether to render one has to be the check for whether
  // there IS one.
  if (!measuredSizes(result)) {
    parts.push(errorDiagnosticsMarkdown(result, confidence.badge));
    return parts.filter(Boolean).join("\n\n");
  }

  parts.push(importResultSizeMarkdown(result, config.compression));
  parts.push(analysisMarkdown(result, runtime, confidence.badge));

  const confidenceNotes = confidenceNotesMarkdown(result.confidence_reasons);

  if (confidenceNotes) {
    parts.push(confidenceNotes);
  }

  const conservativeSizing = conservativeSizingMarkdown(result);

  if (conservativeSizing) {
    parts.push(conservativeSizing);
  }

  if (insights.length > 0) {
    parts.push(["**Insights**", ...insights.map((insight) => `- ${insight.tooltip}`)].join("\n"));
  }

  if (result.diagnostics.length > 0) {
    parts.push(copyDiagnosticsMarkdown(result));
  }

  return parts.filter(Boolean).join("\n\n");
};
