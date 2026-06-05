import path from "node:path";
import type { DetectedImport } from "../imports/types.js";
import type { ConfidenceLevel, ImportResult } from "../ipc/protocol.js";

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
  confidence: ConfidenceLevel | "unknown";
  confidenceReasons: string;
  topModules: string;
  warning: string;
}

export interface WorkspaceReportTreemapItem {
  packageName: string;
  specifier: string;
  sourceFile: string;
  brotliBytes: number;
  percentage: number;
  confidence: ConfidenceLevel | "unknown";
}

export interface WorkspaceReportSummary {
  importCount: number;
  totalBrotliBytes: number;
  lowConfidenceCount: number;
  mediumConfidenceCount: number;
  conservativeCount: number;
  treemap: WorkspaceReportTreemapItem[];
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
        confidence: confidenceForResult(result),
        confidenceReasons: confidenceReasonsForResult(result),
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

export const buildReportSummary = (rows: readonly WorkspaceReportRow[]): WorkspaceReportSummary => {
  const totalBrotliBytes = rows.reduce((total, row) => total + row.brotliBytes, 0);
  const treemap = rows
    .filter((row) => row.brotliBytes > 0)
    .slice(0, 10)
    .map((row) => ({
      packageName: row.packageName,
      specifier: row.specifier,
      sourceFile: row.sourceFile,
      brotliBytes: row.brotliBytes,
      percentage: totalBrotliBytes > 0 ? Math.round((row.brotliBytes / totalBrotliBytes) * 100) : 0,
      confidence: row.confidence,
    }));

  return {
    importCount: rows.length,
    totalBrotliBytes,
    lowConfidenceCount: rows.filter((row) => row.confidence === "low").length,
    mediumConfidenceCount: rows.filter((row) => row.confidence === "medium").length,
    conservativeCount: rows.filter((row) => row.warning.includes("Conservative estimate")).length,
    treemap,
  };
};

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

const confidenceForResult = (result: ImportResult | undefined): ConfidenceLevel | "unknown" =>
  result?.confidence ?? "unknown";

const confidenceReasonsForResult = (result: ImportResult | undefined): string =>
  result?.confidence_reasons.join(" · ") ?? "";

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

  if (item.result?.confidence === "low") {
    warnings.push(`Low confidence${confidenceReasonSuffix(item.result)}`);
  } else if (item.result?.confidence === "medium") {
    warnings.push(`Medium confidence${confidenceReasonSuffix(item.result)}`);
  }

  return warnings.join(" · ");
};

const confidenceReasonSuffix = (result: ImportResult): string => {
  if (result.confidence_reasons.length === 0) {
    return "";
  }

  return `: ${result.confidence_reasons.join(" · ")}`;
};
