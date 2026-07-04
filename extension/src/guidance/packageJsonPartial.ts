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
  if (!partial.indexes) {
    return mergePackageJsonFinalStates(currentStates, partial.states);
  }

  const nextStates = [...currentStates];

  partial.indexes.forEach((stateIndex, partialIndex) => {
    const incoming = partial.states[partialIndex];

    if (!incoming) {
      return;
    }

    const current = nextStates[stateIndex];

    if (current && !isSameDependencyState(current, incoming)) {
      return;
    }

    nextStates[stateIndex] = mergePackageJsonState(current, incoming);
  });

  return nextStates;
};

export const mergePackageJsonFinalStates = (
  currentStates: readonly PackageJsonMergeState[],
  finalStates: readonly PackageJsonDependencyAnalysisItem[],
): PackageJsonMergeState[] =>
  finalStates.map((incoming, index) => mergePackageJsonState(currentStates[index], incoming));

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
  if (!current) {
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

const isSameDependencyState = (
  current: PackageJsonDependencyAnalysisItem,
  incoming: PackageJsonDependencyAnalysisItem,
): boolean =>
  current.name === incoming.name &&
  current.section === incoming.section &&
  current.entry.name === incoming.entry.name;
