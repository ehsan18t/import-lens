import * as vscode from "vscode";
import type { ImportAnalysisInsight } from "../analysis/state.js";
import type { ImportResult } from "../ipc/protocol.js";
import type { ImportRuntime } from "../imports/types.js";
import { getImportLensConfig } from "../config.js";
import { confidenceVisualFor } from "./confidenceVisuals.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { formatBytes, type CompressionFormat } from "./format.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";

const appendCopyDiagnosticsLink = (tooltip: vscode.MarkdownString, result: ImportResult): void => {
  const args = encodeURIComponent(JSON.stringify([result]));
  tooltip.isTrusted = { enabledCommands: [copyImportDiagnosticsCommand] };
  tooltip.appendMarkdown(`[$(copy) Copy diagnostics](command:${copyImportDiagnosticsCommand}?${args})`);
};

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

export const tooltipForResult = (
  result: ImportResult,
  runtime: ImportRuntime = "component",
  insights: readonly ImportAnalysisInsight[] = [],
): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);
  const confidence = confidenceVisualFor(result.confidence);
  tooltip.appendMarkdown(`**${result.specifier}**\n\n`);

  if (result.error) {
    tooltip.appendMarkdown("ImportLens could not compute this import size.\n\n");
    tooltip.appendMarkdown(`**Confidence:** **${confidence.badge}**\n\n`);
    for (const reason of result.confidence_reasons) {
      tooltip.appendMarkdown(`- ${reason}\n`);
    }
    if (result.confidence_reasons.length > 0) {
      tooltip.appendMarkdown("\n");
    }
    appendCopyDiagnosticsLink(tooltip, result);
    return tooltip;
  }

  const selected = selectedCompressionSize(result, getImportLensConfig().compression);
  tooltip.appendMarkdown(`**Selected ${selected.label}: ${selected.value}**\n\n`);
  tooltip.appendMarkdown(`Raw: ${formatBytes(result.raw_bytes)}\n\n`);
  tooltip.appendMarkdown(`Minified: ${formatBytes(result.minified_bytes)}\n\n`);
  tooltip.appendMarkdown(`Gzip: ${formatBytes(result.gzip_bytes)}\n\n`);
  tooltip.appendMarkdown(`Brotli: ${formatBytes(result.brotli_bytes)}\n\n`);
  tooltip.appendMarkdown(`Zstd: ${formatBytes(result.zstd_bytes)}\n\n`);
  if (result.shared_bytes && result.shared_bytes > 0) {
    tooltip.appendMarkdown(`Shared in file: ${formatBytes(result.shared_bytes)}\n\n`);
  }
  if (isTypesOnlyResult(result)) {
    tooltip.appendMarkdown("**Type-only package:** yes\n\n");
  }
  tooltip.appendMarkdown(`**Confidence:** **${confidence.badge}**\n\n`);
  for (const reason of result.confidence_reasons) {
    tooltip.appendMarkdown(`- ${reason}\n`);
  }
  if (result.confidence_reasons.length > 0) {
    tooltip.appendMarkdown("\n");
  }
  tooltip.appendMarkdown("**Status**\n\n");
  tooltip.appendMarkdown(`- Runtime: ${runtime}\n`);
  tooltip.appendMarkdown(`- Side effects: ${result.side_effects ? "yes" : "no"}\n`);
  tooltip.appendMarkdown(`- CommonJS: ${result.is_cjs ? "yes" : "no"}\n`);
  tooltip.appendMarkdown(`- Tree-shakeable: ${result.truly_treeshakeable ? "yes" : "no"}\n`);
  if (insights.length > 0) {
    tooltip.appendMarkdown("\n\n**Insights**\n\n");
    for (const insight of insights) {
      tooltip.appendMarkdown(`- ${insight.tooltip}\n`);
    }
  }
  if (result.diagnostics.length > 0) {
    tooltip.appendMarkdown("\n\n");
    appendCopyDiagnosticsLink(tooltip, result);
  }
  return tooltip;
};

export const tooltipForMessage = (title: string, message: string): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**${title}**\n\n`);
  tooltip.appendText(message);
  return tooltip;
};
