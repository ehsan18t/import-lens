import type { ImportAnalysisState } from "../analysis/state.js";

export const shouldOfferNamedExportCandidates = (state: ImportAnalysisState): boolean => {
  const result = state.result;

  if (state.status !== "ready" || !result || result.error) {
    return false;
  }

  return state.detected.importKind === "namespace" &&
    !result.truly_treeshakeable;
};
