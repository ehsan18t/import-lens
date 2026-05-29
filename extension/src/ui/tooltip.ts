import * as vscode from "vscode";
import type { ImportResult } from "../ipc/protocol.js";

export const tooltipForResult = (result: ImportResult): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**${result.specifier}**\n\n`);

  if (result.error) {
    tooltip.appendMarkdown("ImportLens could not compute this import size.\n\n");
    tooltip.appendText(result.error);
    return tooltip;
  }

  tooltip.appendMarkdown(`Raw: ${result.raw_bytes} B\n\n`);
  tooltip.appendMarkdown(`Minified: ${result.minified_bytes} B\n\n`);
  tooltip.appendMarkdown(`Gzip: ${result.gzip_bytes} B\n\n`);
  tooltip.appendMarkdown(`Brotli: ${result.brotli_bytes} B\n\n`);
  tooltip.appendMarkdown(`Zstd: ${result.zstd_bytes} B\n\n`);
  tooltip.appendMarkdown(`Side effects: ${result.side_effects ? "yes" : "no"}\n\n`);
  tooltip.appendMarkdown(`CJS: ${result.is_cjs ? "yes" : "no"}`);
  return tooltip;
};

export const tooltipForMessage = (title: string, message: string): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**${title}**\n\n`);
  tooltip.appendText(message);
  return tooltip;
};
