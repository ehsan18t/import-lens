import type { ImportAnalysisItem } from "../ipc/protocol.js";
import type { ImportAnalysisState } from "./state.js";

/**
 * Turn one daemon `ImportAnalysisItem` into the UI state for that import.
 *
 * `loading` is the interesting one. The daemon no longer waits for an import's engine
 * build before answering: a cold import comes back `loading`, and its size arrives
 * later on the `refreshed_results` push, which `mergeRefreshedResults` merges into
 * THIS state. So the state has to exist — a pushed result can update an import's state
 * but never create one, and an import missing from the response is dropped outright
 * (the store is rebuilt from `response.imports` on every analysis).
 *
 * It must also read as "measuring", never as an error and never as zero bytes:
 * `importHintParts` renders a `loading` state as "Calculating...". Before this it fell
 * through to `unavailable` — "Daemon unavailable", rendered as no hint at all.
 *
 * Kept vscode-free so the mapping is unit-testable under the repo's `node --test`
 * harness; `listener.ts` is not.
 */
export const importAnalysisStateFromDaemon = (
  item: ImportAnalysisItem,
  logMissingResult: (specifier: string, reason: string) => void,
): ImportAnalysisState => {
  if (item.status === "ready" && item.result) {
    return {
      detected: item.detected,
      status: "ready",
      result: item.result,
    };
  }

  if (item.status === "loading") {
    return {
      detected: item.detected,
      status: "loading",
    };
  }

  if (item.status === "missing") {
    logMissingResult(item.detected.specifier, item.message ?? "Package not found");
    return {
      detected: item.detected,
      status: "missing",
      message: item.message ?? "Package not found",
    };
  }

  return {
    detected: item.detected,
    status: "unavailable",
    message: item.message ?? "Daemon unavailable",
  };
};
