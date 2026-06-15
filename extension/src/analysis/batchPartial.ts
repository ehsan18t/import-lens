import type { BatchResponse, ImportResult } from "../ipc/protocol.js";

export interface StreamingBatchPartialOptions<TState> {
  readonly requestId: number;
  readonly isCurrent: (requestId: number) => boolean;
  readonly requestStateIndexes: readonly number[];
  readonly states: readonly TState[];
  readonly isMissing: (state: TState) => boolean;
  readonly matchesResult: (state: TState, result: ImportResult) => boolean;
  readonly applyReady: (state: TState, result: ImportResult) => TState;
  readonly commit: (states: readonly TState[]) => void;
}

export const applyStreamingBatchPartial = <TState>(
  partial: BatchResponse,
  options: StreamingBatchPartialOptions<TState>,
): readonly TState[] | null => {
  if (!options.isCurrent(partial.request_id) || !partial.indexes) {
    return null;
  }

  const nextStates = [...options.states];

  partial.indexes.forEach((requestImportIndex, partialIndex) => {
    const stateIndex = options.requestStateIndexes[requestImportIndex];
    const state = stateIndex === undefined ? undefined : nextStates[stateIndex];
    const result = partial.imports[partialIndex];

    if (!state || options.isMissing(state) || !result || !options.matchesResult(state, result)) {
      return;
    }

    nextStates[stateIndex] = options.applyReady(state, result);
  });

  options.commit(nextStates);
  return nextStates;
};
