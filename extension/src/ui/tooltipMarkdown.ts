import type { ImportAnalysisInsight } from "../analysis/state.js";
import type { ImportLensConfig } from "../config.js";
import type { ImportResult, ImportRuntime } from "../ipc/protocol.js";
import { confidenceVisualFor } from "./confidenceVisuals.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import {
  bytesForCompression,
  type CompressionFormat,
  formatBytes,
  labelForCompression,
  type MeasuredSizes,
  measuredSizes,
} from "./format.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";

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
  ].join("\n");
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

  return rows.join("\n");
};

const errorDiagnosticsMarkdown = (result: ImportResult, confidenceBadge: string): string => {
  const rows = [
    "**Diagnostics**",
    "Import Lens could not compute this import size.",
    `- Error: ${result.error}`,
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

  if (result.error) {
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
