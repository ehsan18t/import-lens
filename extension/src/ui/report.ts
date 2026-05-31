import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { buildReportRows } from "../report/reportModel.js";
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
  const panel = vscode.window.createWebviewPanel("importLensReport", "ImportLens Report", vscode.ViewColumn.Beside, {
    enableScripts: false,
  });
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
table{border-collapse:collapse;width:100%}
td,th{border-bottom:1px solid var(--vscode-panel-border);padding:6px 8px;text-align:left;vertical-align:top}
th{font-weight:600}
.empty{color:var(--vscode-descriptionForeground)}
</style>
</head>
<body>
<h1>ImportLens Workspace Report</h1>
<table>
<thead><tr><th>Package</th><th>Import</th><th>Source</th><th>Line</th><th>Runtime</th><th>Minified</th><th>Gzip</th><th>Brotli</th><th>Zstd</th><th>Shared</th><th>Top Modules</th><th>Warning</th></tr></thead>
<tbody>${rows || `<tr><td class="empty" colspan="12">No package imports found.</td></tr>`}</tbody>
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
