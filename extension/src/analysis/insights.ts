import path from "node:path";
import type { ImportRuntime } from "../ipc/protocol.js";
import { formatBytes, measuredSizes } from "../ui/format.js";
import { budgetInsightForState, type ImportLensBudgets } from "./budgets.js";
import {
  type ImportCostHistoryItem,
  importCostHistoryDeltaLabel,
  importCostHistoryItem,
  previousImportCostFor,
} from "./history.js";
import { importIdentityKey, importIdentityLabel, importIdentityOf } from "./importIdentity.js";
import type { ImportAnalysisInsight, ImportAnalysisState } from "./state.js";

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

/**
 * "These bytes are shared, and here is who with."
 *
 * The daemon computes `shared_bytes` **per result**, and this named the sharers **per specifier** —
 * so `import React, { useState } from "react"`, which is one specifier and TWO results, found no
 * module with more than one "sharer" and told the user the shared bytes were *"outside the public
 * top-module breakdown"*. They were not: the sharer was the sibling import on the same line, and the
 * module was right there in the breakdown. Keyed by identity, both halves of that statement are
 * found, and the sibling is NAMED by its identity too — naming it by specifier would render
 * "…also appears in ;" with an empty list, because both results say "react".
 */
const sharedDependencyInsight = (
  state: ImportAnalysisState,
  sharedModules: SharedModuleIndex,
): ImportAnalysisInsight | null => {
  const result = state.result;
  const sharedBytes = result?.shared_bytes ?? 0;

  if (!result || sharedBytes <= 0) {
    return null;
  }

  // Sharers are looked up within THIS import's runtime. An import in another runtime pulling the
  // same module is not a sharer: it ships its own copy, so it saves this import nothing (ADR-0005).
  const sharersOf = (modulePath: string): ReadonlyMap<string, string> =>
    sharedModules.get(sharedModuleKey(state.detected.runtime, modulePath)) ??
    new Map<string, string>();

  const shared = (result.module_breakdown ?? [])
    .filter((module) => sharersOf(module.path).size > 1)
    .slice(0, 3);

  // Still true, and still said: the wire carries only the top 10 modules, so bytes the daemon knows
  // are shared can be shared by a module this side never sees. That case was never the lie — the
  // specifier collision was — and a message that is right half the time is not deleted.
  if (shared.length === 0) {
    return {
      tooltip: `Shared dependency: ${formatBytes(sharedBytes)} is shared with other imports in this file outside the public top-module breakdown.`,
    };
  }

  const self = importIdentityKey(importIdentityOf(state.detected));
  const otherImports = new Set<string>();

  for (const module of shared) {
    for (const [identity, label] of sharersOf(module.path)) {
      if (identity !== self) {
        otherImports.add(label);
      }
    }
  }

  const modules = shared.map((module) => path.basename(module.path)).join(", ");
  const others = [...otherImports].sort().join(", ");

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

/**
 * Which imports pull each module in: module (within a runtime) -> the IMPORTS that reach it, keyed
 * by {@link importIdentityKey} and valued by the label the user is shown.
 *
 * **Indexed by import, not by specifier**, because the daemon shares by RESULT and one specifier can
 * be two imports (see {@link sharedDependencyInsight}). And **within a runtime**, because a runtime
 * is an artifact boundary (ADR-0005) and that is the only place a module is genuinely shared: a
 * module reached from Astro frontmatter (server) and from a client `<script>` was counted as shared,
 * and `sharedDependencyInsight` sold that to the user as a deduplication saving the build model
 * explicitly does not perform — false on exactly the file shape the runtime split exists to handle.
 */
type SharedModuleIndex = ReadonlyMap<string, Map<string, string>>;

const sharedModuleKey = (runtime: ImportRuntime, modulePath: string): string =>
  `${runtime}\u0000${modulePath}`;

const sharedModuleIndex = (states: readonly ImportAnalysisState[]): SharedModuleIndex => {
  const modules = new Map<string, Map<string, string>>();

  for (const state of states) {
    if (state.status !== "ready" || !measuredSizes(state.result)) {
      continue;
    }

    const identity = importIdentityOf(state.detected);

    for (const module of state.result?.module_breakdown ?? []) {
      const key = sharedModuleKey(state.detected.runtime, module.path);
      const importers = modules.get(key) ?? new Map<string, string>();
      importers.set(importIdentityKey(identity), importIdentityLabel(identity));
      modules.set(key, importers);
    }
  }

  return modules;
};
