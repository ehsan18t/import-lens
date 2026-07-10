import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import { protocolVersion, type WorkspaceReportSummary } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { Logger } from "../logging/types.js";
import { confidenceCssColor, confidenceVisualFor } from "./confidenceVisuals.js";
import { formatBytes } from "./format.js";

export const showReport = async (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
): Promise<void> => {
  logger.info("Building workspace report.");
  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

  if (!workspaceRoot) {
    await vscode.window.showWarningMessage("Import Lens report requires an open workspace folder.");
    return;
  }

  if (daemon.state !== "ready" && (await daemon.start(workspaceRoot)) !== "ready") {
    await vscode.window.showWarningMessage("Import Lens daemon is unavailable.");
    return;
  }

  try {
    const config = getImportLensConfig();
    const response = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Import Lens: Building workspace report",
      },
      () =>
        daemon.requestWorkspaceReport({
          type: "workspace_report",
          version: protocolVersion,
          request_id: nextIpcRequestId(),
          workspace_root: workspaceRoot,
          budgets: config.budgets,
        }),
    );

    if (!response || response.error) {
      await vscode.window.showWarningMessage(
        `Import Lens report unavailable${response?.error ? `: ${response.error}` : "."}`,
      );
      return;
    }

    logger.info(`Workspace report built with ${response.rows.length} import item(s).`);
    const reportRows = response.rows;
    const summary = response.summary;
    const panel = vscode.window.createWebviewPanel(
      "importLensReport",
      "Import Lens Report",
      vscode.ViewColumn.Beside,
      {
        enableScripts: false,
      },
    );
    const treemap = svgTreemap(summary.treemap);
    const confidenceLegend = (["high", "medium", "low"] as const)
      .map((confidence) => {
        const visual = confidenceVisualFor(confidence);
        return `<span class="legend-item ${visual.cssClass}"><span class="legend-swatch"></span>${visual.label}</span>`;
      })
      .join("");
    const duplicateImports = summary.duplicateImports
      .map(
        (item) =>
          `<tr><td>${escapeHtml(item.specifier)}</td><td>${item.count}</td><td>${formatBytes(item.totalBrotliBytes)}</td><td>${escapeHtml(item.sourceFiles.join(", "))}</td></tr>`,
      )
      .join("");
    const sharedModules = summary.sharedModules
      .map(
        (item) =>
          `<tr><td>${escapeHtml(item.basename)}</td><td>${item.count}</td><td>${formatBytes(item.totalBytes)}</td><td>${escapeHtml(item.specifiers.join(", "))}</td><td>${item.vendored ? "yes" : "no"}</td><td>${escapeHtml(item.modulePath)}</td></tr>`,
      )
      .join("");
    const rows = reportRows
      .map(
        (row) => `<tr>
<td>${escapeHtml(row.packageName)}</td>
<td>${escapeHtml(row.specifier)}</td>
<td>${escapeHtml(row.sourceFile)}</td>
<td>${row.line}</td>
<td>${escapeHtml(row.runtime)}</td>
<td>${formatBytes(row.minifiedBytes)}</td>
<td>${formatBytes(row.gzipBytes)}</td>
<td>${formatBytes(row.brotliBytes)}</td>
<td>${formatBytes(row.zstdBytes)}</td>
<td>${formatBytes(row.sharedBytes)}</td>
<td class="confidence ${confidenceVisualFor(row.confidence).cssClass}">${escapeHtml(row.confidence)}</td>
<td>${escapeHtml(row.confidenceReasons)}</td>
<td>${escapeHtml(row.topModules)}</td>
<td>${escapeHtml(row.warning)}</td>
</tr>`,
      )
      .join("");

    panel.webview.html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
body{font-family:var(--vscode-font-family);padding:16px;color:var(--vscode-foreground)}
.summary{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:8px;margin:0 0 16px}
.metric{border:1px solid var(--vscode-panel-border);padding:8px}
.metric strong{display:block;font-size:18px;margin-top:2px}
.treemap{margin:0 0 16px;max-width:100%;height:auto;background:var(--vscode-editorWidget-background)}
.treemap text{fill:var(--vscode-editor-foreground);font-size:12px}
.legend{display:flex;gap:12px;flex-wrap:wrap;margin:0 0 12px}
.legend-item{display:inline-flex;align-items:center;gap:6px;font-weight:600}
.legend-swatch{width:10px;height:10px;background:currentColor;border-radius:2px}
.confidence{font-weight:600}
.confidence-high{color:${confidenceCssColor("high")}}
.confidence-medium{color:${confidenceCssColor("medium")}}
.confidence-low{color:${confidenceCssColor("low")}}
.confidence-unknown{color:${confidenceCssColor("unknown")}}
.confidence-fill-high{fill:${confidenceCssColor("high")}}
.confidence-fill-medium{fill:${confidenceCssColor("medium")}}
.confidence-fill-low{fill:${confidenceCssColor("low")}}
.confidence-fill-unknown{fill:${confidenceCssColor("unknown")}}
table{border-collapse:collapse;width:100%}
td,th{border-bottom:1px solid var(--vscode-panel-border);padding:6px 8px;text-align:left;vertical-align:top}
th{font-weight:600}
.empty{color:var(--vscode-descriptionForeground)}
</style>
</head>
<body>
<h1>Import Lens Workspace Report</h1>
<section class="summary">
<div class="metric">Imports<strong>${summary.importCount}</strong></div>
<div class="metric">Total Brotli<strong>${formatBytes(summary.totalBrotliBytes)}</strong></div>
<div class="metric">Low confidence<strong>${summary.lowConfidenceCount}</strong></div>
<div class="metric">Medium confidence<strong>${summary.mediumConfidenceCount}</strong></div>
<div class="metric">Conservative<strong>${summary.conservativeCount}</strong></div>
<div class="metric">Budget violations<strong>${summary.budgetViolationCount}</strong></div>
</section>
<section class="legend">${confidenceLegend}</section>
<section>${treemap || `<p class="empty">No measured imports to summarize.</p>`}</section>
<h2>Duplicate Imports</h2>
<table>
<thead><tr><th>Import</th><th>Count</th><th>Total Brotli</th><th>Sources</th></tr></thead>
<tbody>${duplicateImports || `<tr><td class="empty" colspan="4">No duplicate import specifiers found.</td></tr>`}</tbody>
</table>
<h2>Shared Modules</h2>
<table>
<thead><tr><th>Module</th><th>Count</th><th>Total Bytes</th><th>Imports</th><th>Vendored</th><th>Path</th></tr></thead>
<tbody>${sharedModules || `<tr><td class="empty" colspan="6">No shared top modules found.</td></tr>`}</tbody>
</table>
<h2>Imports</h2>
<table>
<thead><tr><th>Package</th><th>Import</th><th>Source</th><th>Line</th><th>Runtime</th><th>Minified</th><th>Gzip</th><th>Brotli</th><th>Zstd</th><th>Shared</th><th>Confidence</th><th>Confidence Reasons</th><th>Top Modules</th><th>Warning</th></tr></thead>
<tbody>${rows || `<tr><td class="empty" colspan="14">No package imports found.</td></tr>`}</tbody>
</table>
</body>
</html>`;
    context.subscriptions.push(panel);
  } catch (error) {
    logger.warn(
      `Workspace report request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    await vscode.window.showWarningMessage(
      `Import Lens report request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
};

const escapeHtml = (value: string): string =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

const svgTreemap = (items: WorkspaceReportSummary["treemap"]): string => {
  if (items.length === 0) {
    return "";
  }

  const width = 1000;
  const rowHeight = 28;
  const height = items.length * rowHeight;
  const rows = items
    .map((item, index) => {
      const y = index * rowHeight;
      const fillClass = confidenceVisualFor(item.confidence).cssClass.replace(
        "confidence-",
        "confidence-fill-",
      );
      const barWidth = Math.max(1, Math.round((item.percentage / 100) * width));
      return `<g>
<rect class="${fillClass}" x="0" y="${y}" width="${barWidth}" height="24"></rect>
<text x="8" y="${y + 17}">${escapeHtml(item.specifier)} · ${formatBytes(item.brotliBytes)} br · ${item.percentage}%</text>
</g>`;
    })
    .join("");

  return `<svg class="treemap" viewBox="0 0 ${width} ${height}" role="img" aria-label="Brotli size treemap">${rows}</svg>`;
};
