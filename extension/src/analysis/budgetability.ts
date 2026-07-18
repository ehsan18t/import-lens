import type { FileSizeDocumentResponse, ImportDiagnostic, ImportResult } from "../ipc/protocol.js";
import { isDurableFileSize, isDurableImportResult } from "./transience.js";

/**
 * Deterministic measurements that are safe to cache but are not precise enough for a pass/fail
 * verdict. This list mirrors `NON_BUDGETABLE_RESULT_STAGES` in the daemon and the standalone CLI;
 * a script-level coordination test keeps all three copies aligned.
 */
export const nonBudgetableResultStages: readonly string[] = ["imprecise_assets"];

const nonBudgetableStages = new Set(nonBudgetableResultStages);

export const hasNonBudgetableDiagnostic = (
  diagnostics: readonly ImportDiagnostic[] | undefined,
): boolean => (diagnostics ?? []).some((diagnostic) => nonBudgetableStages.has(diagnostic.stage));

/** A measured import that is both durable and precise enough to compare with a threshold. */
export const isBudgetableImportResult = (
  result: ImportResult | undefined,
): result is ImportResult =>
  isDurableImportResult(result) && !hasNonBudgetableDiagnostic(result.diagnostics);

/** A File Cost that is both durable and precise enough to compare with a threshold. */
export const isBudgetableFileSize = (
  response: Pick<FileSizeDocumentResponse, "error" | "diagnostics" | "incomplete" | "degraded">,
): boolean => isDurableFileSize(response) && !hasNonBudgetableDiagnostic(response.diagnostics);
