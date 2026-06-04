import path from "node:path";
import type { DetectedImport } from "../imports/types.js";
import type { ImportResult } from "../ipc/protocol.js";

export interface WorkspaceReportItem {
  detected: DetectedImport;
  sourceFile: string;
  workspaceRoot: string;
  result?: ImportResult;
  warning?: string;
}

export interface WorkspaceReportRow {
  packageName: string;
  specifier: string;
  sourceFile: string;
  line: number;
  runtime: string;
  minifiedBytes: number;
  gzipBytes: number;
  brotliBytes: number;
  zstdBytes: number;
  sharedBytes: number;
  topModules: string;
  warning: string;
}

export const buildReportRows = (items: readonly WorkspaceReportItem[]): WorkspaceReportRow[] =>
  items
    .map((item) => {
      const result = item.result;

      return {
        packageName: item.detected.packageName,
        specifier: item.detected.specifier,
        sourceFile: relativeSourceFile(item.workspaceRoot, item.sourceFile),
        line: item.detected.line + 1,
        runtime: item.detected.runtime,
        minifiedBytes: result?.minified_bytes ?? 0,
        gzipBytes: result?.gzip_bytes ?? 0,
        brotliBytes: result?.brotli_bytes ?? 0,
        zstdBytes: result?.zstd_bytes ?? 0,
        sharedBytes: result?.shared_bytes ?? 0,
        topModules: moduleBreakdownSummary(result),
        warning: warningForItem(item),
      };
    })
    .sort((left, right) => {
      const sizeDelta = right.brotliBytes - left.brotliBytes;

      if (sizeDelta !== 0) {
        return sizeDelta;
      }

      return `${left.sourceFile}:${left.line}:${left.specifier}`.localeCompare(
        `${right.sourceFile}:${right.line}:${right.specifier}`,
      );
    });

const relativeSourceFile = (workspaceRoot: string, sourceFile: string): string => {
  const relative = path.relative(workspaceRoot, sourceFile);
  return (relative || sourceFile).split(path.sep).join("/");
};

const moduleBreakdownSummary = (result: ImportResult | undefined): string => {
  const modules = result?.module_breakdown ?? [];

  return modules
    .slice(0, 3)
    .map((module) => `${path.basename(module.path)} (${module.bytes} B)`)
    .join(", ");
};

const warningForItem = (item: WorkspaceReportItem): string => {
  const warnings: string[] = [];

  if (item.warning) {
    warnings.push(item.warning);
  }

  if (item.result?.error) {
    warnings.push(item.result.error);
  }

  if (item.result?.shared_bytes && item.result.shared_bytes > 0) {
    warnings.push(`Shares ${item.result.shared_bytes} B with other imports in this file`);
  }

  if (item.result?.is_cjs || item.result?.side_effects || item.result?.truly_treeshakeable === false) {
    warnings.push("Conservative estimate");
  }

  return warnings.join(" · ");
};
