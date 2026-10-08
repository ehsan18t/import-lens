import type {
  AnalyzePackageJsonResponse,
  PackageJsonDependencyAnalysisItem,
  RegistryHint,
} from "../ipc/protocol.js";
import type { RegistryHintRefreshStatus } from "./packageJsonState.js";

type PackageJsonRefreshStateFields = {
  registryHintRefreshStatus?: RegistryHintRefreshStatus;
  registryHintRefreshError?: string | null;
};

type PackageJsonMergeState = PackageJsonDependencyAnalysisItem & PackageJsonRefreshStateFields;

export const mergePackageJsonAnalysisPartial = (
  currentStates: readonly PackageJsonMergeState[],
  partial: AnalyzePackageJsonResponse,
): PackageJsonMergeState[] => {
  if (!partial.indexes || isDependencySkeleton(partial)) {
    return mergePackageJsonFinalStates(currentStates, partial.states);
  }

  const nextStates = [...currentStates];

  partial.indexes.forEach((stateIndex, partialIndex) => {
    const incoming = partial.states[partialIndex];

    if (!incoming) {
      return;
    }

    // The index is a position in the CURRENT request's dependency list, so whatever sits there
    // now is replaced; it carries over only what belongs to the same dependency.
    const current = nextStates[stateIndex];
    nextStates[stateIndex] =
      current && isSameDependencyState(current, incoming)
        ? mergePackageJsonState(current, incoming)
        : incoming;
  });

  return nextStates;
};

/**
 * The daemon's first partial of a request: every dependency of the manifest as it is now, loading,
 * with the sections (later partials carry no sections). It replaces the previous list rather than
 * patching it by index, because an added, removed or re-sorted dependency shifts every index after
 * it: patched by position, each line showed its neighbour's size until the final response.
 */
const isDependencySkeleton = (partial: AnalyzePackageJsonResponse): boolean =>
  partial.sections.length > 0 &&
  partial.indexes !== undefined &&
  partial.indexes.length === partial.states.length &&
  partial.indexes.every((stateIndex, position) => stateIndex === position);

// Matched by identity, never by index: adding or re-sorting a dependency shifts every index
// after it, and a hint is only true of the package it was fetched for.
export const mergePackageJsonFinalStates = (
  currentStates: readonly PackageJsonMergeState[],
  finalStates: readonly PackageJsonDependencyAnalysisItem[],
): PackageJsonMergeState[] => {
  const currentByIdentity = new Map<string, PackageJsonMergeState>();

  for (const state of currentStates) {
    const key = dependencyIdentityKey(state);

    if (!currentByIdentity.has(key)) {
      currentByIdentity.set(key, state);
    }
  }

  return finalStates.map((incoming) =>
    mergePackageJsonState(currentByIdentity.get(dependencyIdentityKey(incoming)), incoming),
  );
};

export const markPackageJsonLoadingUnavailable = (
  states: readonly PackageJsonDependencyAnalysisItem[],
  message: string,
): PackageJsonDependencyAnalysisItem[] =>
  states.map((state) =>
    state.status === "loading"
      ? {
          ...state,
          status: "unavailable",
          message,
        }
      : state,
  );

const mergePackageJsonState = (
  current: PackageJsonMergeState | undefined,
  incoming: PackageJsonDependencyAnalysisItem,
): PackageJsonMergeState => {
  if (!current || !isSameInstalledVersion(current, incoming)) {
    return incoming;
  }

  const registryHint = newerRegistryHint(current.registryHint, incoming.registryHint);

  return {
    ...incoming,
    registryHint,
    registryHintRefreshStatus: current.registryHintRefreshStatus,
    registryHintRefreshError: current.registryHintRefreshError,
  };
};

export const newerRegistryHint = (
  current: RegistryHint | null | undefined,
  incoming: RegistryHint | null | undefined,
): RegistryHint | null | undefined => {
  if (incoming === undefined || incoming === null) {
    return current;
  }

  if (current === undefined || current === null) {
    return incoming;
  }

  const currentFetchedAt = current.fetchedAt ?? 0;
  const incomingFetchedAt = incoming.fetchedAt ?? 0;

  return currentFetchedAt > incomingFetchedAt ? current : incoming;
};

const dependencyIdentityKey = (state: PackageJsonDependencyAnalysisItem): string =>
  `${state.section}
${state.name}`;

// A hint's latest/update verdict is computed against the installed version, so it only carries
// across states that agree on it. An unknown version (the daemon's names-only loading rows) is not
// a disagreement: dropping the hint there would blank it on every re-analysis.
const isSameInstalledVersion = (
  current: PackageJsonDependencyAnalysisItem,
  incoming: PackageJsonDependencyAnalysisItem,
): boolean =>
  current.installedVersion === undefined ||
  incoming.installedVersion === undefined ||
  current.installedVersion === incoming.installedVersion;

const isSameDependencyState = (
  current: PackageJsonDependencyAnalysisItem,
  incoming: PackageJsonDependencyAnalysisItem,
): boolean =>
  current.name === incoming.name &&
  current.section === incoming.section &&
  current.entry.name === incoming.entry.name;

export type PackageJsonFinalResponseOutcome = "apply" | "clear" | "keep";

// The daemon answers JSON it cannot parse with no states and no error, exactly as it answers a
// manifest with no dependencies. Clearing on the first would blank every annotation each time an
// edit passes through invalid JSON (typing the comma before a new dependency line), so only a
// manifest that really parses clears; an unparseable one keeps the last good view.
export const packageJsonFinalResponseOutcome = (
  response: Pick<AnalyzePackageJsonResponse, "error" | "states">,
  source: string,
): PackageJsonFinalResponseOutcome => {
  if (response.error) {
    return "clear";
  }

  if (response.states.length > 0) {
    return "apply";
  }

  return isParseableJson(source) ? "clear" : "keep";
};

const isParseableJson = (source: string): boolean => {
  try {
    JSON.parse(source);
    return true;
  } catch {
    return false;
  }
};
