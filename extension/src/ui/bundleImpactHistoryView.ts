import { bundleImpactHistoryDeltaLabel, type BundleImpactHistoryItem } from "../analysis/history.js";
import { formatBytes } from "./format.js";

export const bundleImpactHistoryHtml = (history: readonly BundleImpactHistoryItem[]): string => {
  const newestFirst = [...history].sort((left, right) => right.timestamp - left.timestamp);
  const oldestFirst = [...newestFirst].reverse();
  const maxBrotli = Math.max(...oldestFirst.map((item) => item.brotliBytes), 1);
  const rows = newestFirst.map((item) => historyRowHtml(item)).join("");

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>ImportLens Bundle Impact History</title>
<style>
:root {
  color-scheme: light dark;
  --importlens-accent: var(--vscode-charts-blue);
  --importlens-border: var(--vscode-panel-border);
}
body {
  margin: 0;
  padding: 24px;
  color: var(--vscode-foreground);
  background: var(--vscode-editor-background);
  font-family: var(--vscode-font-family);
}
h1 {
  margin: 0 0 16px;
  font-size: 20px;
  font-weight: 600;
}
.summary {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  margin-bottom: 18px;
}
.metric {
  border: 1px solid var(--importlens-border);
  border-radius: 6px;
  padding: 10px 12px;
  min-width: 140px;
}
.metric strong {
  display: block;
  margin-top: 4px;
  font-size: 18px;
}
svg {
  display: block;
  width: 100%;
  max-width: 960px;
  height: 220px;
  margin-bottom: 20px;
  border: 1px solid var(--importlens-border);
  border-radius: 6px;
  background: color-mix(in srgb, var(--vscode-editor-background) 92%, var(--vscode-foreground));
}
table {
  width: 100%;
  border-collapse: collapse;
}
th,
td {
  padding: 9px 8px;
  border-bottom: 1px solid var(--importlens-border);
  text-align: left;
  vertical-align: top;
}
th {
  color: var(--vscode-descriptionForeground);
  font-size: 12px;
  font-weight: 600;
  text-transform: uppercase;
}
.file {
  word-break: break-all;
}
.bytes {
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
</style>
</head>
<body>
<h1>Bundle Impact History</h1>
${historySummaryHtml(newestFirst)}
${historyChartSvg(oldestFirst, maxBrotli)}
<table>
<thead>
<tr>
<th>Measured</th>
<th>File</th>
<th>Imports</th>
<th>Brotli</th>
<th>Gzip</th>
<th>Minified</th>
</tr>
</thead>
<tbody>${rows}</tbody>
</table>
</body>
</html>`;
};

const historySummaryHtml = (history: readonly BundleImpactHistoryItem[]): string => {
  const latest = history[0];
  const previous = latest
    ? history.find((item) => item.fileName === latest.fileName && item.timestamp !== latest.timestamp)
    : undefined;
  const delta = latest && previous ? bundleImpactHistoryDeltaLabel(latest, previous) : "No previous match";

  return `<section class="summary" aria-label="Bundle impact summary">
<div class="metric">Latest Brotli<strong>${latest ? formatBytes(latest.brotliBytes) : "0 B"}</strong></div>
<div class="metric">Latest Imports<strong>${latest?.importCount ?? 0}</strong></div>
<div class="metric">Latest Delta<strong>${escapeHtml(delta)}</strong></div>
</section>`;
};

const historyRowHtml = (item: BundleImpactHistoryItem): string => `
<tr>
<td>${escapeHtml(new Date(item.timestamp).toLocaleString())}</td>
<td class="file">${escapeHtml(item.fileName)}</td>
<td class="bytes">${item.importCount}</td>
<td class="bytes">${formatBytes(item.brotliBytes)}</td>
<td class="bytes">${formatBytes(item.gzipBytes)}</td>
<td class="bytes">${formatBytes(item.minifiedBytes)}</td>
</tr>`;

const historyChartSvg = (
  history: readonly BundleImpactHistoryItem[],
  maxBrotli: number,
): string => {
  const width = 960;
  const height = 220;
  const padding = 28;
  const chartWidth = width - padding * 2;
  const chartHeight = height - padding * 2;
  const points = history.map((item, index) => {
    const x = padding + (history.length === 1 ? chartWidth / 2 : (index / (history.length - 1)) * chartWidth);
    const y = padding + chartHeight - (item.brotliBytes / maxBrotli) * chartHeight;
    return { item, x, y };
  });
  const polyline = points.map((point) => `${point.x.toFixed(1)},${point.y.toFixed(1)}`).join(" ");
  const circles = points.map((point) => `
<circle cx="${point.x.toFixed(1)}" cy="${point.y.toFixed(1)}" r="4">
<title>${escapeHtml(`${formatBytes(point.item.brotliBytes)} br - ${point.item.fileName}`)}</title>
</circle>`).join("");

  return `<svg role="img" aria-label="Brotli size trend" viewBox="0 0 ${width} ${height}">
<line x1="${padding}" y1="${height - padding}" x2="${width - padding}" y2="${height - padding}" stroke="var(--vscode-panel-border)" />
<line x1="${padding}" y1="${padding}" x2="${padding}" y2="${height - padding}" stroke="var(--vscode-panel-border)" />
<text x="${padding}" y="18" fill="var(--vscode-descriptionForeground)" font-size="12">${escapeHtml(formatBytes(maxBrotli))} br</text>
<text x="${padding}" y="${height - 8}" fill="var(--vscode-descriptionForeground)" font-size="12">0 B</text>
<polyline points="${polyline}" fill="none" stroke="var(--importlens-accent)" stroke-width="3" />
<g fill="var(--importlens-accent)">${circles}</g>
</svg>`;
};

const escapeHtml = (value: string): string =>
  value
    .replace(/&/gu, "&amp;")
    .replace(/</gu, "&lt;")
    .replace(/>/gu, "&gt;")
    .replace(/"/gu, "&quot;")
    .replace(/'/gu, "&#39;");
