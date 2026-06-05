import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { buildReportRows, buildReportSummary } from "../report/reportModel.js";
import { buildWorkspaceReportItems, type WorkspaceScannerApi } from "../report/workspaceScanner.js";
import { formatBytes } from "./format.js";

export const showReport = async (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
): Promise<void> => {
  const items = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Building workspace report",
    },
    () => buildWorkspaceReportItems(workspaceScannerApi(), daemon),
  );
  const reportRows = buildReportRows(items);
  const summary = buildReportSummary(reportRows);
  const panel = vscode.window.createWebviewPanel("importLensReport", "ImportLens Report", vscode.ViewColumn.Beside, {
    enableScripts: false,
  });
  const treemap = summary.treemap
    .map((item) => `<div class="bar">
<div class="bar-fill" style="width:${item.percentage}%"></div>
<div class="bar-label">${escapeHtml(item.specifier)} · ${formatBytes(item.brotliBytes)} br · ${item.percentage}%</div>
</div>`)
    .join("");
  const rows = reportRows
    .map((row) => `<tr>
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
<td>${escapeHtml(row.confidence)}</td>
<td>${escapeHtml(row.confidenceReasons)}</td>
<td>${escapeHtml(row.topModules)}</td>
<td>${escapeHtml(row.warning)}</td>
</tr>`)
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
.bars{margin:0 0 16px}
.bar{position:relative;height:24px;margin:4px 0;background:var(--vscode-editorWidget-background);overflow:hidden}
.bar-fill{position:absolute;inset:0 auto 0 0;background:var(--vscode-progressBar-background)}
.bar-label{position:relative;padding:4px 8px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
table{border-collapse:collapse;width:100%}
td,th{border-bottom:1px solid var(--vscode-panel-border);padding:6px 8px;text-align:left;vertical-align:top}
th{font-weight:600}
.empty{color:var(--vscode-descriptionForeground)}
</style>
</head>
<body>
<h1>ImportLens Workspace Report</h1>
<section class="summary">
<div class="metric">Imports<strong>${summary.importCount}</strong></div>
<div class="metric">Total Brotli<strong>${formatBytes(summary.totalBrotliBytes)}</strong></div>
<div class="metric">Low confidence<strong>${summary.lowConfidenceCount}</strong></div>
<div class="metric">Medium confidence<strong>${summary.mediumConfidenceCount}</strong></div>
<div class="metric">Conservative<strong>${summary.conservativeCount}</strong></div>
</section>
<section class="bars">${treemap || `<p class="empty">No measured imports to summarize.</p>`}</section>
<table>
<thead><tr><th>Package</th><th>Import</th><th>Source</th><th>Line</th><th>Runtime</th><th>Minified</th><th>Gzip</th><th>Brotli</th><th>Zstd</th><th>Shared</th><th>Confidence</th><th>Confidence Reasons</th><th>Top Modules</th><th>Warning</th></tr></thead>
<tbody>${rows || `<tr><td class="empty" colspan="14">No package imports found.</td></tr>`}</tbody>
</table>
</body>
</html>`;
  context.subscriptions.push(panel);
};

const workspaceScannerApi = (): WorkspaceScannerApi => ({
  findFiles: async (include, exclude) => vscode.workspace.findFiles(include, exclude),
  openTextDocument: async (uri) => vscode.workspace.openTextDocument(uri as vscode.Uri),
  getWorkspaceFolder: (uri) => vscode.workspace.getWorkspaceFolder(uri as vscode.Uri),
});

const escapeHtml = (value: string): string =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
