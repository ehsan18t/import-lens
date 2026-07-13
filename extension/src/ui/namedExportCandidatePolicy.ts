import type { ImportAnalysisState } from "../analysis/state.js";
import { measuredSizes } from "./format.js";

export const shouldOfferNamedExportCandidates = (state: ImportAnalysisState): boolean => {
  const result = state.result;

  // "Is there a size?", never "is there an error?" (ADR-0006, invariant 2). `truly_treeshakeable` is
  // a verdict a build reached; without a build there is no verdict to act on, and offering to narrow
  // a namespace import of a package nobody could measure is advice from nothing.
  if (state.status !== "ready" || !measuredSizes(result)) {
    return false;
  }

  return state.detected.importKind === "namespace" && result?.truly_treeshakeable !== true;
};
