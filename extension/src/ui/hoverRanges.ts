import type { ImportAnalysisState } from "../analysis/state.js";
import type { SourcePosition, SourceRange } from "../imports/types.js";

const compareSourcePositions = (left: SourcePosition, right: SourcePosition): number => {
  if (left.line !== right.line) {
    return left.line - right.line;
  }

  return left.character - right.character;
};

export const sourceRangeContainsPosition = (
  range: SourceRange,
  position: SourcePosition,
): boolean =>
  compareSourcePositions(position, range.start) >= 0
  && compareSourcePositions(position, range.end) < 0;

export const stateForHoverPosition = (
  states: readonly ImportAnalysisState[],
  position: SourcePosition,
): ImportAnalysisState | undefined =>
  states.find((state) => sourceRangeContainsPosition(state.detected.statementRange, position));
