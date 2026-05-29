import type { ImportAnalysisState } from "./state.js";

export const markLoadingStatesUnavailable = (
  states: ImportAnalysisState[],
  message: string,
): ImportAnalysisState[] =>
  states.map((state) => {
    if (state.status !== "loading") {
      return state;
    }

    return {
      detected: state.detected,
      status: "unavailable",
      message,
    };
  });
