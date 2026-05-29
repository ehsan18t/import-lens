import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { formatBytes } from "./format.js";

export const showReport = (context: vscode.ExtensionContext, store: AnalysisStore): void => {
  const panel = vscode.window.createWebviewPanel("importLensReport", "ImportLens Report", vscode.ViewColumn.Beside, {
    enableScripts: false,
  });
  const rows = store
    .all()
    .filter((state) => state.result)
    .sort((left, right) => (right.result?.brotli_bytes ?? 0) - (left.result?.brotli_bytes ?? 0))
    .map((state) => {
      const result = state.result!;
      return `<tr><td>${escapeHtml(result.specifier)}</td><td>${formatBytes(result.minified_bytes)}</td><td>${formatBytes(result.gzip_bytes)}</td><td>${formatBytes(result.brotli_bytes)}</td><td>${formatBytes(result.zstd_bytes)}</td></tr>`;
    })
    .join("");

  panel.webview.html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
body{font-family:var(--vscode-font-family);padding:16px;color:var(--vscode-foreground)}
table{border-collapse:collapse;width:100%}
td,th{border-bottom:1px solid var(--vscode-panel-border);padding:6px 8px;text-align:left}
</style>
</head>
<body>
<h1>ImportLens Report</h1>
<table>
<thead><tr><th>Import</th><th>Minified</th><th>Gzip</th><th>Brotli</th><th>Zstd</th></tr></thead>
<tbody>${rows}</tbody>
</table>
</body>
</html>`;
  context.subscriptions.push(panel);
};

const escapeHtml = (value: string): string =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

