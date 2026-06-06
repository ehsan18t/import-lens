import type { ImportAnalysisState } from "./state.js";
import type { ImportResult } from "../ipc/protocol.js";

export type MissingResultLogger = (specifier: string, reason: string) => void;

export const markLoadingStatesUnavailable = (
  states: readonly ImportAnalysisState[],
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

export const applyFinalBatchResults = (
  states: readonly ImportAnalysisState[],
  results: readonly ImportResult[],
  logMissingResult: MissingResultLogger,
): ImportAnalysisState[] => {
  let responseIndex = 0;

  return states.map((state) => {
    if (state.status === "missing") {
      return state;
    }

    const result = results[responseIndex++];

    if (!result) {
      logMissingResult(
        state.detected.specifier,
        "daemon response did not include a matching result",
      );
      return preserveReadyOrMarkUnavailable(state, "No daemon response");
    }

    if (result.specifier !== state.detected.specifier) {
      logMissingResult(
        state.detected.specifier,
        `daemon response returned ${result.specifier} instead`,
      );
      return preserveReadyOrMarkUnavailable(state, "No daemon response");
    }

    return {
      detected: state.detected,
      status: "ready",
      result,
    };
  });
};

const preserveReadyOrMarkUnavailable = (
  state: ImportAnalysisState,
  message: string,
): ImportAnalysisState => {
  if (state.status === "ready" && state.result) {
    return state;
  }

  return {
    detected: state.detected,
    status: "unavailable",
    message,
  };
};
