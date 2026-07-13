import path from "node:path";
import { formatBytes, measuredSizes } from "../ui/format.js";
import { budgetInsightForState, type ImportLensBudgets } from "./budgets.js";
import {
  type ImportCostHistoryItem,
  importCostHistoryDeltaLabel,
  importCostHistoryItem,
  previousImportCostFor,
} from "./history.js";
import type { ImportAnalysisInsight, ImportAnalysisState } from "./state.js";
import { isDurableImportResult } from "./transience.js";

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
    // Every insight below is a claim about a size — a delta, a budget, a trend, a share. An
    // import with no size supports none of them, and the guard asks exactly that.
    if (state.status !== "ready" || !measuredSizes(state.result)) {
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

export const insightLabelSuffix = (
  insights: readonly ImportAnalysisInsight[] | undefined,
): string => {
  const labels = (insights ?? [])
    .map((insight) => insight.label)
    .filter((label): label is string => Boolean(label));

  return labels.length > 0 ? ` · ${labels.join(" · ")}` : "";
};

/**
 * The rows a document's states contribute to the PERSISTED import-cost history.
 *
 * `isDurableImportResult` is the whole reason this is a filter and not a map. The history keeps one
 * row per import identity, with no TTL and no cache generation, so a row that should not be there
 * does not merely go stale — it overwrites that import's real baseline permanently, and every later
 * trend ("was 17 KB, now 58 B") is measured against a number that never happened. Two results must
 * be refused: one with no size (nothing to record), and one whose measurement was degraded by a
 * transient failure of the daemon rather than a fact about the package.
 */
export const importCostHistoryItemsForStates = (
  states: readonly ImportAnalysisState[],
  now: number = Date.now(),
): ImportCostHistoryItem[] =>
  states
    .filter(
      (
        state,
      ): state is ImportAnalysisState & { result: NonNullable<ImportAnalysisState["result"]> } =>
        state.status === "ready" && isDurableImportResult(state.result),
    )
    .map((state) => importCostHistoryItem(state.detected, state.result, now))
    .filter((item): item is ImportCostHistoryItem => item !== undefined);

/**
 * "This import adds N bytes to your working-tree change."
 *
 * It guarded `!state.result` and not the SIZE, so an import with no size rendered
 * **"+NaN kB br"** — `formatBytes(undefined)` all the way to the CodeLens. Now it simply does not
 * claim a delta it cannot compute.
 */
const gitDeltaInsight = (
  state: ImportAnalysisState,
  changedLines: ReadonlySet<number> | undefined,
): ImportAnalysisInsight | null => {
  const sizes = measuredSizes(state.result);

  if (!changedLines || changedLines.size === 0 || !sizes) {
    return null;
  }

  for (
    let line = state.detected.statementRange.start.line;
    line <= state.detected.statementRange.end.line;
    line += 1
  ) {
    if (changedLines.has(line)) {
      return {
        label: `+${formatBytes(sizes.brotli_bytes)} br`,
        tooltip: `Working-tree change: this import currently adds ${formatBytes(sizes.brotli_bytes)} brotli.`,
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
  if (state.detected.syntax !== "star_reexport" || !measuredSizes(state.result)) {
    return null;
  }

  return {
    label: "barrel",
    tooltip: `Barrel re-export: export * from '${state.detected.specifier}' keeps the package boundary broad and can prevent named-export tree-shaking. Prefer named re-exports when possible.`,
  };
};

/**
 * "This import was 17 kB last time; it is 2.5 kB now."
 *
 * Two bugs, one gate. It guarded `!state.result` and not the size, so it rendered
 * **"was 17.5 kB br, now NaN kB br"**. And it computed a delta between a DURABLE baseline and a
 * *degraded* current result — inventing a regression, or a win, that never happened. A trend is a
 * comparison of two measurements, so it needs two (ADR-0006, invariant 4).
 */
const historyTrendInsight = (
  state: ImportAnalysisState,
  history: readonly ImportCostHistoryItem[],
  now: number | undefined,
): ImportAnalysisInsight | null => {
  const result = state.result;

  if (!result || !measuredSizes(result)) {
    return null;
  }

  const previous = previousImportCostFor(history, state.detected);
  if (!previous) {
    return null;
  }

  const current = importCostHistoryItem(state.detected, result, now);
  if (!current) {
    return null;
  }

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
    if (state.status !== "ready" || !measuredSizes(state.result)) {
      continue;
    }

    for (const module of state.result?.module_breakdown ?? []) {
      const specifiers = modules.get(module.path) ?? new Set<string>();
      specifiers.add(state.detected.specifier);
      modules.set(module.path, specifiers);
    }
  }

  return modules;
};
