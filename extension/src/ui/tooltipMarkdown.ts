import type { ImportAnalysisInsight } from "../analysis/state.js";
import type { ImportLensConfig } from "../config.js";
import type { ImportResult } from "../ipc/protocol.js";
import type { ImportRuntime } from "../imports/types.js";
import { confidenceVisualFor } from "./confidenceVisuals.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { formatBytes, type CompressionFormat } from "./format.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";

const selectedCompressionSize = (
  result: ImportResult,
  compression: CompressionFormat,
): { label: string; value: string } => {
  if (compression === "gzip") {
    return { label: "Gzip", value: `${formatBytes(result.gzip_bytes)} gz` };
  }

  if (compression === "zstd") {
    return { label: "Zstd", value: `${formatBytes(result.zstd_bytes)} zstd` };
  }

  return { label: "Brotli", value: `${formatBytes(result.brotli_bytes)} br` };
};

export const copyDiagnosticsMarkdown = (result: ImportResult): string => {
  const args = encodeURIComponent(JSON.stringify([result]));
  return `[$(copy) Copy diagnostics](command:${copyImportDiagnosticsCommand}?${args})`;
};

export const resultHasDiagnosticsLink = (result: ImportResult): boolean =>
  Boolean(result.error) || result.diagnostics.length > 0;

export const importResultSizeMarkdown = (
  result: ImportResult,
  compression: CompressionFormat,
): string => {
  const selected = selectedCompressionSize(result, compression);

  return [
    `**Selected ${selected.label}: ${selected.value}**`,
    `Raw: ${formatBytes(result.raw_bytes)}`,
    `Minified: ${formatBytes(result.minified_bytes)}`,
    `Gzip: ${formatBytes(result.gzip_bytes)}`,
    `Brotli: ${formatBytes(result.brotli_bytes)}`,
    `Zstd: ${formatBytes(result.zstd_bytes)}`,
  ].join("\n\n");
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
    parts.push("ImportLens could not compute this import size.");
    parts.push(`**Confidence:** **${confidence.badge}**`);

    if (result.confidence_reasons.length > 0) {
      parts.push(result.confidence_reasons.map((reason) => `- ${reason}`).join("\n"));
    }

    parts.push(copyDiagnosticsMarkdown(result));
    return parts.filter(Boolean).join("\n\n");
  }

  parts.push(importResultSizeMarkdown(result, config.compression));

  if (result.shared_bytes && result.shared_bytes > 0) {
    parts.push(`Shared in file: ${formatBytes(result.shared_bytes)}`);
  }

  if (isTypesOnlyResult(result)) {
    parts.push("**Type-only package:** yes");
  }

  parts.push(`**Confidence:** **${confidence.badge}**`);

  if (result.confidence_reasons.length > 0) {
    parts.push(result.confidence_reasons.map((reason) => `- ${reason}`).join("\n"));
  }

  parts.push([
    "**Status**",
    `- Runtime: ${runtime}`,
    `- Side effects: ${result.side_effects ? "yes" : "no"}`,
    `- CommonJS: ${result.is_cjs ? "yes" : "no"}`,
    `- Tree-shakeable: ${result.truly_treeshakeable ? "yes" : "no"}`,
  ].join("\n"));

  if (insights.length > 0) {
    parts.push([
      "**Insights**",
      ...insights.map((insight) => `- ${insight.tooltip}`),
    ].join("\n"));
  }

  if (result.diagnostics.length > 0) {
    parts.push(copyDiagnosticsMarkdown(result));
  }

  return parts.filter(Boolean).join("\n\n");
};
