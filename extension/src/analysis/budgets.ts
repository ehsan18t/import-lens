import type { SourceRange } from "../ipc/protocol.js";
import { formatBytes, measuredSizes } from "../ui/format.js";
import { isBudgetableFileSize, isBudgetableImportResult } from "./budgetability.js";
import type { DocumentFileCost } from "./fileSize.js";
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
  const sizes = isBudgetableImportResult(state.result) ? measuredSizes(state.result) : null;

  if (limit === undefined || state.status !== "ready" || !sizes || sizes.brotli_bytes <= limit) {
    return null;
  }

  return {
    label: "over budget",
    tooltip: `Budget: ${state.detected.specifier} is ${formatBytes(sizes.brotli_bytes)} br, over the per-import budget of ${formatBytes(limit)} br.`,
  };
};

/**
 * The three answers the per-file budget has, and "not evaluated" is a first-class one.
 *
 * The file budget is judged against the **File Cost** — the daemon's ONE combined build over all
 * the document's imports, in which a module two of them reach is counted once (ADR-0004). It used to
 * be judged against the SUM of the per-import costs, which is a *Combined Import Cost*: an upper
 * bound that counts a shared graph once per import, and a quantity no file ever ships. Five
 * `@mui/material` subpath imports at 40 kB each build to 55 kB together, and the editor warned that
 * file as 200 kB — 3x over a 60 kB budget — while the status bar, one line away, showed 55 kB.
 *
 * And when the File Cost is **not the file's number** — `incomplete` (a floor: an import that
 * belongs in it contributed no bytes) or `degraded` (the file's own combined build failed, so what
 * is left is that same over-counting sum) — the answer is neither "over" nor "under". It is *not
 * evaluated*: ADR-0006 invariant 5 forbids a false FAIL exactly as firmly as a false pass. The
 * predicate is {@link isDurableFileSize}, the same one the daemon's aggregate cache and the
 * extension's persisted history apply, and deliberately not a second reading of the flags.
 */
export type FileBudgetVerdict = "not-evaluated" | "within-budget" | "over-budget";

export const fileBudgetVerdict = (
  budgets: ImportLensBudgets,
  fileCost: DocumentFileCost | undefined,
): FileBudgetVerdict => {
  const limit = budgets.perFileBrotliBytes;

  if (limit === undefined || !fileCost || !isBudgetableFileSize(fileCost)) {
    return "not-evaluated";
  }

  return fileCost.brotliBytes > limit ? "over-budget" : "within-budget";
};

export const budgetViolationsForStates = (
  states: readonly ImportAnalysisState[],
  budgets: ImportLensBudgets,
  fileCost?: DocumentFileCost,
): BudgetViolation[] => {
  const violations: BudgetViolation[] = [];
  const importLimit = budgets.perImportBrotliBytes;
  const fileLimit = budgets.perFileBrotliBytes;

  for (const state of states) {
    const sizes = isBudgetableImportResult(state.result) ? measuredSizes(state.result) : null;

    if (state.status !== "ready" || !sizes) {
      continue;
    }

    const actualBytes = sizes.brotli_bytes;

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

  // Anchored on the file's FIRST import, measured or not: the File Cost is one build over all of
  // them, so no single import owns the verdict — and the one this used to hang on ("the first
  // MEASURED import") was chosen by the accumulation that no longer exists.
  const fileRange: SourceRange | undefined = states[0]?.detected.statementRange;

  if (
    fileLimit !== undefined &&
    fileCost &&
    fileRange &&
    fileBudgetVerdict(budgets, fileCost) === "over-budget"
  ) {
    violations.push({
      kind: "file",
      actualBytes: fileCost.brotliBytes,
      limitBytes: fileLimit,
      range: fileRange,
      message: `Import Lens file budget exceeded: this file's imports build to ${formatBytes(fileCost.brotliBytes)} br, over ${formatBytes(fileLimit)} br.`,
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
