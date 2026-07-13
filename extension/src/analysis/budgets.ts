import type { SourceRange } from "../ipc/protocol.js";
import { formatBytes, measuredSizes } from "../ui/format.js";
import type { ImportAnalysisInsight, ImportAnalysisState } from "./state.js";

export interface ImportLensBudgets {
  perImportBrotliBytes?: number;
  perFileBrotliBytes?: number;
}

export type BudgetViolationKind = "import" | "file";

export interface BudgetViolation {
  kind: BudgetViolationKind;
  message: string;
  actualBytes: number;
  limitBytes: number;
  range: SourceRange;
  specifier?: string;
}

export const sanitizeBudgets = (value: unknown): ImportLensBudgets => {
  if (!value || typeof value !== "object") {
    return {};
  }

  const candidate = value as Record<string, unknown>;

  return {
    ...positiveIntegerBudget(candidate.perImportBrotliBytes, "perImportBrotliBytes"),
    ...positiveIntegerBudget(candidate.perFileBrotliBytes, "perFileBrotliBytes"),
  };
};

export const hasBudgets = (budgets: ImportLensBudgets): boolean =>
  budgets.perImportBrotliBytes !== undefined || budgets.perFileBrotliBytes !== undefined;

/**
 * "Over budget" — or nothing at all.
 *
 * There is no third answer, and that is the point: an import with no size is **not evaluated**, and
 * silence here means only that no violation was established. The old gate asked `!result.error`, so
 * a transiently-degraded import — `error: null`, fabricated 58-byte size — read as comfortably
 * UNDER budget, and the warning it had been carrying was quietly withdrawn from the Problems panel
 * on the very run where the daemon knew the least about it. No verdict from a floor (ADR-0006,
 * invariant 5).
 */
export const budgetInsightForState = (
  state: ImportAnalysisState,
  budgets: ImportLensBudgets,
): ImportAnalysisInsight | null => {
  const limit = budgets.perImportBrotliBytes;
  const sizes = measuredSizes(state.result);

  if (limit === undefined || state.status !== "ready" || !sizes || sizes.brotli_bytes <= limit) {
    return null;
  }

  return {
    label: "over budget",
    tooltip: `Budget: ${state.detected.specifier} is ${formatBytes(sizes.brotli_bytes)} br, over the per-import budget of ${formatBytes(limit)} br.`,
  };
};

export const budgetViolationsForStates = (
  states: readonly ImportAnalysisState[],
  budgets: ImportLensBudgets,
): BudgetViolation[] => {
  const violations: BudgetViolation[] = [];
  const importLimit = budgets.perImportBrotliBytes;
  const fileLimit = budgets.perFileBrotliBytes;
  let fileBrotliBytes = 0;
  let firstMeasuredRange: SourceRange | undefined;

  for (const state of states) {
    const sizes = measuredSizes(state.result);

    if (state.status !== "ready" || !sizes) {
      continue;
    }

    const actualBytes = sizes.brotli_bytes;
    fileBrotliBytes += actualBytes;
    firstMeasuredRange ??= state.detected.statementRange;

    if (importLimit !== undefined && actualBytes > importLimit) {
      violations.push({
        kind: "import",
        specifier: state.detected.specifier,
        actualBytes,
        limitBytes: importLimit,
        range: state.detected.statementRange,
        message: `Import Lens budget exceeded: ${state.detected.specifier} is ${formatBytes(actualBytes)} br, over ${formatBytes(importLimit)} br.`,
      });
    }
  }

  if (fileLimit !== undefined && firstMeasuredRange && fileBrotliBytes > fileLimit) {
    violations.push({
      kind: "file",
      actualBytes: fileBrotliBytes,
      limitBytes: fileLimit,
      range: firstMeasuredRange,
      message: `Import Lens file budget exceeded: analyzed imports total ${formatBytes(fileBrotliBytes)} br, over ${formatBytes(fileLimit)} br.`,
    });
  }

  return violations;
};

const positiveIntegerBudget = (value: unknown, key: keyof ImportLensBudgets): ImportLensBudgets => {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return {};
  }

  return { [key]: Math.floor(value) };
};
