import path from "node:path";
import {
  importCostHistoryDeltaLabel,
  importCostHistoryItem,
  previousImportCostFor,
  type ImportCostHistoryItem,
} from "./history.js";
import { budgetInsightForState, type ImportLensBudgets } from "./budgets.js";
import type { ImportAnalysisInsight, ImportAnalysisState } from "./state.js";
import { formatBytes } from "../ui/format.js";

export interface ImportAnalysisInsightOptions {
  changedLines?: ReadonlySet<number>;
  importCostHistory: readonly ImportCostHistoryItem[];
  budgets?: ImportLensBudgets;
  now?: number;
}

export const applyImportAnalysisInsights = (
  states: readonly ImportAnalysisState[],
  options: ImportAnalysisInsightOptions,
): ImportAnalysisState[] => {
  const sharedModules = sharedModuleIndex(states);

  return states.map((state) => {
    if (state.status !== "ready" || !state.result || state.result.error) {
      return state;
    }

    const insights = [
      gitDeltaInsight(state, options.changedLines),
      budgetInsightForState(state, options.budgets ?? {}),
      sharedDependencyInsight(state, sharedModules),
      barrelReexportInsight(state),
      historyTrendInsight(state, options.importCostHistory, options.now),
    ].filter((insight): insight is ImportAnalysisInsight => Boolean(insight));

    // Insights are derived entirely from the current state and options, so
    // recompute and replace rather than append: reapplying (e.g. on a UI-only
    // config change over already-insighted stored states) must not accumulate
    // duplicate tags, and must clear insights whose inputs no longer produce
    // them (e.g. an "over budget" tag after the budget is raised).
    if (insights.length === 0) {
      if (!state.insights || state.insights.length === 0) {
        return state;
      }
      return { ...state, insights: undefined };
    }

    return { ...state, insights };
  });
};

export const insightLabelSuffix = (insights: readonly ImportAnalysisInsight[] | undefined): string => {
  const labels = (insights ?? [])
    .map((insight) => insight.label)
    .filter((label): label is string => Boolean(label));

  return labels.length > 0 ? ` · ${labels.join(" · ")}` : "";
};

export const importCostHistoryItemsForStates = (
  states: readonly ImportAnalysisState[],
  now: number = Date.now(),
): ImportCostHistoryItem[] =>
  states
    .filter((state): state is ImportAnalysisState & { result: NonNullable<ImportAnalysisState["result"]> } =>
      state.status === "ready" && Boolean(state.result) && !state.result?.error)
    .map((state) => importCostHistoryItem(state.detected, state.result, now));

const gitDeltaInsight = (
  state: ImportAnalysisState,
  changedLines: ReadonlySet<number> | undefined,
): ImportAnalysisInsight | null => {
  if (!changedLines || changedLines.size === 0 || !state.result) {
    return null;
  }

  for (let line = state.detected.statementRange.start.line; line <= state.detected.statementRange.end.line; line += 1) {
    if (changedLines.has(line)) {
      return {
        label: `+${formatBytes(state.result.brotli_bytes)} br`,
        tooltip: `Working-tree change: this import currently adds ${formatBytes(state.result.brotli_bytes)} brotli.`,
      };
    }
  }

  return null;
};

const sharedDependencyInsight = (
  state: ImportAnalysisState,
  sharedModules: Map<string, Set<string>>,
): ImportAnalysisInsight | null => {
  const result = state.result;
  const sharedBytes = result?.shared_bytes ?? 0;

  if (!result || sharedBytes <= 0) {
    return null;
  }

  const shared = (result.module_breakdown ?? [])
    .filter((module) => (sharedModules.get(module.path)?.size ?? 0) > 1)
    .slice(0, 3);

  if (shared.length === 0) {
    return {
      tooltip: `Shared dependency: ${formatBytes(sharedBytes)} is shared with other imports in this file outside the public top-module breakdown.`,
    };
  }

  const otherSpecifiers = new Set<string>();

  for (const module of shared) {
    for (const specifier of sharedModules.get(module.path) ?? []) {
      if (specifier !== state.detected.specifier) {
        otherSpecifiers.add(specifier);
      }
    }
  }

  const modules = shared.map((module) => path.basename(module.path)).join(", ");
  const others = [...otherSpecifiers].sort().join(", ");

  return {
    tooltip: `Shared dependency: ${modules} also appears in ${others}; shared bytes in this file: ${formatBytes(sharedBytes)}.`,
  };
};

const barrelReexportInsight = (state: ImportAnalysisState): ImportAnalysisInsight | null => {
  if (state.detected.syntax !== "star_reexport" || !state.result || state.result.error) {
    return null;
  }

  return {
    label: "barrel",
    tooltip: `Barrel re-export: export * from '${state.detected.specifier}' keeps the package boundary broad and can prevent named-export tree-shaking. Prefer named re-exports when possible.`,
  };
};

const historyTrendInsight = (
  state: ImportAnalysisState,
  history: readonly ImportCostHistoryItem[],
  now: number | undefined,
): ImportAnalysisInsight | null => {
  if (!state.result) {
    return null;
  }

  const previous = previousImportCostFor(history, state.detected);
  if (!previous) {
    return null;
  }

  const current = importCostHistoryItem(state.detected, state.result, now);
  const delta = current.brotliBytes - previous.brotliBytes;

  if (delta === 0) {
    return null;
  }

  return {
    tooltip: `History: ${state.detected.specifier} was ${formatBytes(previous.brotliBytes)} br, now ${formatBytes(current.brotliBytes)} br (${importCostHistoryDeltaLabel(current, previous)}).`,
  };
};

const sharedModuleIndex = (states: readonly ImportAnalysisState[]): Map<string, Set<string>> => {
  const modules = new Map<string, Set<string>>();

  for (const state of states) {
    if (state.status !== "ready" || !state.result || state.result.error) {
      continue;
    }

    for (const module of state.result.module_breakdown ?? []) {
      const specifiers = modules.get(module.path) ?? new Set<string>();
      specifiers.add(state.detected.specifier);
      modules.set(module.path, specifiers);
    }
  }

  return modules;
};
