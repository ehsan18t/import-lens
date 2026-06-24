import path from "node:path";
import type { ImportLensBudgets } from "../analysis/budgets.js";
import type { DetectedImport } from "../ipc/protocol.js";
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
  moduleContributions: { path: string; bytes: number }[];
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
  budgetViolationCount: number;
  duplicateImports: DuplicateImportGroup[];
  sharedModules: DuplicateModuleGroup[];
  treemap: WorkspaceReportTreemapItem[];
}

export interface DuplicateImportGroup {
  specifier: string;
  count: number;
  totalBrotliBytes: number;
  sourceFiles: string[];
}

export interface DuplicateModuleGroup {
  modulePath: string;
  basename: string;
  count: number;
  totalBytes: number;
  specifiers: string[];
  vendored: boolean;
}

export const buildReportRows = (
  items: readonly WorkspaceReportItem[],
  budgets: ImportLensBudgets = {},
): WorkspaceReportRow[] => {
  const rows = items
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
        moduleContributions: moduleContributions(result),
        warning: warningForItem(item, budgets),
      };
    });
  return applyFileBudgetWarnings(rows, budgets)
    .sort((left, right) => {
      const sizeDelta = right.brotliBytes - left.brotliBytes;

      if (sizeDelta !== 0) {
        return sizeDelta;
      }

      return `${left.sourceFile}:${left.line}:${left.specifier}`.localeCompare(
        `${right.sourceFile}:${right.line}:${right.specifier}`,
      );
    });
};

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
    budgetViolationCount: rows.filter((row) => /budget exceeded/iu.test(row.warning)).length,
    duplicateImports: buildDuplicateImportGroups(rows),
    sharedModules: buildDuplicateModuleGroups(rows),
    treemap,
  };
};

const applyFileBudgetWarnings = (
  rows: readonly WorkspaceReportRow[],
  budgets: ImportLensBudgets,
): WorkspaceReportRow[] => {
  const limit = budgets.perFileBrotliBytes;

  if (limit === undefined) {
    return [...rows];
  }

  const totals = new Map<string, number>();

  for (const row of rows) {
    if (row.brotliBytes <= 0) {
      continue;
    }

    totals.set(row.sourceFile, (totals.get(row.sourceFile) ?? 0) + row.brotliBytes);
  }

  const warnedFiles = new Set<string>();

  return rows.map((row) => {
    const total = totals.get(row.sourceFile) ?? 0;

    if (total <= limit || warnedFiles.has(row.sourceFile)) {
      return row;
    }

    warnedFiles.add(row.sourceFile);
    return {
      ...row,
      warning: appendWarning(
        row.warning,
        `File budget exceeded: ${total} B br > ${limit} B br`,
      ),
    };
  });
};

export const buildDuplicateImportGroups = (rows: readonly WorkspaceReportRow[]): DuplicateImportGroup[] => {
  const groups = new Map<string, DuplicateImportGroup>();

  for (const row of rows) {
    const group = groups.get(row.specifier) ?? {
      specifier: row.specifier,
      count: 0,
      totalBrotliBytes: 0,
      sourceFiles: [],
    };
    group.count += 1;
    group.totalBrotliBytes += row.brotliBytes;
    group.sourceFiles.push(row.sourceFile);
    groups.set(row.specifier, group);
  }

  return [...groups.values()]
    .filter((group) => group.count > 1)
    .map((group) => ({
      ...group,
      sourceFiles: [...new Set(group.sourceFiles)].sort(),
    }))
    .sort((left, right) =>
      right.count - left.count
      || right.totalBrotliBytes - left.totalBrotliBytes
      || left.specifier.localeCompare(right.specifier));
};

export const buildDuplicateModuleGroups = (rows: readonly WorkspaceReportRow[]): DuplicateModuleGroup[] => {
  const groups = new Map<string, DuplicateModuleGroup>();

  for (const row of rows) {
    for (const module of row.moduleContributions) {
      const group = groups.get(module.path) ?? {
        modulePath: module.path,
        basename: path.basename(module.path),
        count: 0,
        totalBytes: 0,
        specifiers: [],
        vendored: isVendoredModulePath(module.path),
      };
      group.count += 1;
      group.totalBytes += module.bytes;
      group.specifiers.push(row.specifier);
      groups.set(module.path, group);
    }
  }

  return [...groups.values()]
    .filter((group) => group.count > 1)
    .map((group) => ({
      ...group,
      specifiers: [...new Set(group.specifiers)].sort(),
    }))
    .sort((left, right) =>
      right.count - left.count
      || right.totalBytes - left.totalBytes
      || left.modulePath.localeCompare(right.modulePath));
};

const relativeSourceFile = (workspaceRoot: string, sourceFile: string): string => {
  const relative = path.relative(workspaceRoot, sourceFile);
  return (relative || sourceFile).split(path.sep).join("/");
};

const moduleBreakdownSummary = (result: ImportResult | undefined): string => {
  return moduleContributions(result)
    .slice(0, 3)
    .map((module) => `${path.basename(module.path)} (${module.bytes} B)`)
    .join(", ");
};

const moduleContributions = (result: ImportResult | undefined): { path: string; bytes: number }[] =>
  result?.module_breakdown?.map((module) => ({ path: module.path, bytes: module.bytes })) ?? [];

const confidenceForResult = (result: ImportResult | undefined): ConfidenceLevel | "unknown" =>
  result?.confidence ?? "unknown";

const confidenceReasonsForResult = (result: ImportResult | undefined): string =>
  result?.confidence_reasons.join(" · ") ?? "";

const warningForItem = (item: WorkspaceReportItem, budgets: ImportLensBudgets): string => {
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

  if (
    item.result
    && !item.result.error
    && budgets.perImportBrotliBytes !== undefined
    && item.result.brotli_bytes > budgets.perImportBrotliBytes
  ) {
    warnings.push(`Budget exceeded: ${item.result.brotli_bytes} B br > ${budgets.perImportBrotliBytes} B br`);
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

const appendWarning = (existing: string, next: string): string =>
  existing ? `${existing} · ${next}` : next;

const confidenceReasonSuffix = (result: ImportResult): string => {
  if (result.confidence_reasons.length === 0) {
    return "";
  }

  return `: ${result.confidence_reasons.join(" · ")}`;
};

const isVendoredModulePath = (modulePath: string): boolean => {
  const normalized = modulePath.split(path.sep).join("/");
  return /\/vendor(?:ed|s)?\//iu.test(normalized) || /\/node_modules\/.*\/node_modules\//iu.test(normalized);
};
