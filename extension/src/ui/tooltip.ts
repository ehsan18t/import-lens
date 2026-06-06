import * as vscode from "vscode";
import type { ImportAnalysisInsight, ImportAnalysisState } from "../analysis/state.js";
import type { ImportResult } from "../ipc/protocol.js";
import type { ImportRuntime } from "../imports/types.js";
import { getImportLensConfig } from "../config.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { resultHasDiagnosticsLink, tooltipForResultMarkdown } from "./tooltipMarkdown.js";

export const tooltipForResult = (
  result: ImportResult,
  runtime: ImportRuntime = "component",
  insights: readonly ImportAnalysisInsight[] = [],
): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);

  if (resultHasDiagnosticsLink(result)) {
    tooltip.isTrusted = { enabledCommands: [copyImportDiagnosticsCommand] };
  }

  tooltip.appendMarkdown(tooltipForResultMarkdown(result, getImportLensConfig(), runtime, insights));
  return tooltip;
};

export const tooltipForMessage = (title: string, message: string): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**${title}**\n\n`);
  tooltip.appendText(message);
  return tooltip;
};

export const tooltipForAnalysisState = (state: ImportAnalysisState): vscode.MarkdownString | undefined => {
  if (state.status === "missing") {
    return tooltipForMessage("ImportLens", state.message ?? "Package not found");
  }

  if (state.status === "unavailable") {
    return tooltipForMessage("ImportLens", state.message ?? "Daemon unavailable");
  }

  if (state.status === "ready" && state.result) {
    return tooltipForResult(state.result, state.detected.runtime, state.insights);
  }

  return undefined;
};
