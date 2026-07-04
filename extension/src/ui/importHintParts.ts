import type { ImportAnalysisInsight, ImportAnalysisState } from "../analysis/state.js";
import type { ImportLensConfig } from "../config.js";
import { formatImportSizePrimary, importHintTagLabels, importSizePrimaryTone } from "./format.js";
import type { InlineHintParts, InlineHintSuffixPart } from "./inlineHintSegments.js";
import type { InlineHintTone } from "./inlineHintVisuals.js";

const insightToneForLabel = (label: string): InlineHintTone => {
  if (label === "over budget") {
    return "alert";
  }

  if (label === "barrel") {
    return "caution";
  }

  if (label.startsWith("+") || label.startsWith("-")) {
    return "delta";
  }

  return "info";
};

const insightSuffixes = (
  insights: readonly ImportAnalysisInsight[] | undefined,
): InlineHintSuffixPart[] =>
  (insights ?? [])
    .map((insight) => insight.label)
    .filter((label): label is string => Boolean(label))
    .map((label) => ({
      text: label,
      tone: insightToneForLabel(label),
    }));

export const importHintParts = (
  state: ImportAnalysisState,
  config: ImportLensConfig,
): InlineHintParts | null => {
  if (state.status === "loading") {
    return {
      primary: "Calculating...",
      primaryTone: "neutral",
      suffixes: [],
    };
  }

  if (state.status === "missing") {
    return {
      primary: state.message ?? "Package not found",
      primaryTone: "neutral",
      suffixes: [],
    };
  }

  if (state.status === "unavailable") {
    return null;
  }

  if (state.status === "ready" && state.result) {
    const suffixes: InlineHintSuffixPart[] = [
      ...importHintTagLabels(state.result, config.showWarnings, state.detected.runtime).map(
        (text) => ({
          text,
          tone: "tag" as const,
        }),
      ),
      ...insightSuffixes(state.insights),
    ];

    return {
      primary: formatImportSizePrimary(state.result, config),
      primaryTone: state.result.error ? "neutral" : importSizePrimaryTone(state.result.confidence),
      suffixes,
    };
  }

  return null;
};
