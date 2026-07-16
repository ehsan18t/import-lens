import type { WorkspaceReportRow, WorkspaceReportSummary } from "../ipc/protocol.js";
import { confidenceCssColor, confidenceVisualFor } from "./confidenceVisuals.js";
import { formatBytes, formatOptionalBytes } from "./format.js";

export interface WorkspaceReportContent {
  rows: readonly WorkspaceReportRow[];
  summary: WorkspaceReportSummary;
}

/**
 * The headline metric's name, and the whole point of this module existing apart from `report.ts`:
 * the panel's numbers are a **Combined Import Cost** (ADR-0004), and a label is a claim like any
 * other, so it is rendered by a function a test can call.
 */
export const combinedImportCostLabel = "Combined Import Cost";

/**
 * What the reader must be told about the figure, stated in full — because both halves surprise
 * people and only one of them is guessable.
 *
 * The number sums each import measured **on its own**, so a dependency imported in fifty files is
 * counted fifty times. And a single `import React, { useState } from "react"` is **two imports** —
 * one default, one named — which the daemon measures separately and this figure counts twice. That
 * is not a bug to be netted out: subtracting the overlap would assert what the project ships, which
 * needs a bundle model Import Lens deliberately does not have.
 */
export const combinedImportCostNote =
  'Combined Import Cost sums every import measured on its own, as if nothing else were in the app. A dependency imported in several files is counted at every site, and one import React, { useState } from "react" is two imports and is counted twice. It ranks imports and apportions blame — it is not the size your project ships.';

/**
 * The Shared Modules table, which had the same defect one table below the one the headline fix
 * relabelled: it added a module's bytes once per importing row and called the result "Total Bytes",
 * so a 100 kB `react-dom/index.js` reached by three imports rendered as **300 kB**.
 *
 * A module's size and what its importing sites pay are two quantities, and the table now shows both
 * under their own names. The bytes are the module's **rendered** size in the chunk — uncompressed,
 * because a module's contribution to a compressed artifact is not a thing that exists on its own.
 *
 * **And the note must be reconcilable with the two numbers beside it.** It said "Module Bytes is
 * what the module costs at a single site", which invites the reader to multiply: Imports × Module
 * Bytes. That is not the arithmetic, because `module_bytes` is a **max** across the builds that
 * reached the module, not a per-site constant — two builds may tree-shake the same module
 * differently, and the daemon's own test pins it (900 B in one build, 400 B in another → Module
 * Bytes 900, Combined 1.3 kB, Imports 2). The numbers were right and the sentence explaining them
 * was false, which is the same failure as a wrong number: the reader leaves with 1,800.
 */
export const sharedModuleNote =
  "A module reached by more than one import. Every one of those imports is measured on its own, so every one of them pays for it. Module Bytes is the module at its fullest — the largest contribution across the builds that reached it, since two builds may tree-shake it differently. Combined Import Cost adds up what each of those builds actually contributed, so it need not be Module Bytes × Imports. It is an upper bound, never a size. Rendered (uncompressed) bytes.";

export const workspaceReportHtml = ({ rows, summary }: WorkspaceReportContent): string => {
  const confidenceLegend = (["high", "medium", "low"] as const)
    .map((confidence) => {
      const visual = confidenceVisualFor(confidence);
      return `<span class="legend-item ${visual.cssClass}"><span class="legend-swatch"></span>${visual.label}</span>`;
    })
    .join("");
  const duplicateImports = summary.duplicateImports
    .map(
      (item) =>
        `<tr><td>${escapeHtml(item.specifier)}</td><td>${item.count}</td><td>${formatBytes(item.combinedImportCostBrotliBytes)}</td><td>${escapeHtml(item.sourceFiles.join(", "))}</td></tr>`,
    )
    .join("");
  const sharedModules = summary.sharedModules
    .map(
      (item) =>
        `<tr><td>${escapeHtml(item.basename)}</td><td>${item.count}</td><td>${formatBytes(item.moduleBytes)}</td><td>${formatBytes(item.combinedImportCostBytes)}</td><td>${escapeHtml(item.specifiers.join(", "))}</td><td>${item.vendored ? "yes" : "no"}</td><td>${escapeHtml(item.modulePath)}</td></tr>`,
    )
    .join("");
  const importRows = rows
    .map(
      (row) => `<tr>
<td>${escapeHtml(row.packageName)}</td>
<td>${escapeHtml(row.specifier)}</td>
<td>${escapeHtml(row.sourceFile)}</td>
<td>${row.line}</td>
<td>${escapeHtml(row.runtime)}</td>
<td>${formatOptionalBytes(row.minifiedBytes)}</td>
<td>${formatOptionalBytes(row.gzipBytes)}</td>
<td>${formatOptionalBytes(row.brotliBytes)}</td>
<td>${formatOptionalBytes(row.zstdBytes)}</td>
<td>${formatBytes(row.sharedBytes)}</td>
<td class="confidence ${confidenceVisualFor(row.confidence).cssClass}">${escapeHtml(row.confidence)}</td>
<td>${escapeHtml(row.confidenceReasons)}</td>
<td>${escapeHtml(row.topModules)}</td>
<td>${escapeHtml(row.warning)}</td>
</tr>`,
    )
    .join("");
  const treemap = svgTreemap(summary.treemap);

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
body{font-family:var(--vscode-font-family);padding:16px;color:var(--vscode-foreground)}
.summary{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:8px;margin:0 0 8px}
.metric{border:1px solid var(--vscode-panel-border);padding:8px}
.metric strong{display:block;font-size:18px;margin-top:2px}
.note{color:var(--vscode-descriptionForeground);margin:0 0 16px;max-width:80ch}
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
<div class="metric">${combinedImportCostLabel}<strong>${formatBytes(summary.combinedImportCostBrotliBytes)}</strong></div>
<div class="metric">Low confidence<strong>${summary.lowConfidenceCount}</strong></div>
<div class="metric">Medium confidence<strong>${summary.mediumConfidenceCount}</strong></div>
<div class="metric">Conservative<strong>${summary.conservativeCount}</strong></div>
<div class="metric">Budget violations<strong>${summary.budgetViolationCount}</strong></div>
</section>
<p class="note">${escapeHtml(combinedImportCostNote)}</p>
<section class="legend">${confidenceLegend}</section>
<section>${treemap || `<p class="empty">No measured imports to summarize.</p>`}</section>
<h2>Duplicate Imports</h2>
<table>
<thead><tr><th>Import</th><th>Count</th><th>${combinedImportCostLabel}</th><th>Sources</th></tr></thead>
<tbody>${duplicateImports || `<tr><td class="empty" colspan="4">No duplicate import specifiers found.</td></tr>`}</tbody>
</table>
<h2>Shared Modules</h2>
<p class="note">${escapeHtml(sharedModuleNote)}</p>
<table>
<thead><tr><th>Module</th><th>Imports</th><th>Module Bytes</th><th>${combinedImportCostLabel}</th><th>Specifiers</th><th>Vendored</th><th>Path</th></tr></thead>
<tbody>${sharedModules || `<tr><td class="empty" colspan="7">No shared top modules found.</td></tr>`}</tbody>
</table>
<h2>Imports</h2>
<table>
<thead><tr><th>Package</th><th>Import</th><th>Source</th><th>Line</th><th>Runtime</th><th>Minified</th><th>Gzip</th><th>Brotli</th><th>Zstd</th><th>Shared in File</th><th>Confidence</th><th>Confidence Reasons</th><th>Top Modules</th><th>Warning</th></tr></thead>
<tbody>${importRows || `<tr><td class="empty" colspan="14">No package imports found.</td></tr>`}</tbody>
</table>
</body>
</html>`;
};

const escapeHtml = (value: string): string =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

/** Bars sized by each import's share of the Combined Import Cost — a share of a sum, not of a bundle. */
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

  return `<svg class="treemap" viewBox="0 0 ${width} ${height}" role="img" aria-label="${combinedImportCostLabel} share by import">${rows}</svg>`;
};
