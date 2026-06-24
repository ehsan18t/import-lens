import assert from "node:assert/strict";
import test from "node:test";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import { stateForHoverPosition } from "../../src/ui/hoverRanges.js";
import type { SourceRange } from "../../src/ipc/protocol.js";
import { detectedImport } from "../helpers/detectedImport.js";

const state = (
  specifier: string,
  statementRange: SourceRange,
  specifierRange: SourceRange,
): ImportAnalysisState => ({
  status: "loading",
  detected: detectedImport({
    specifier,
    packageName: specifier,
    line: statementRange.start.line,
    quoteEnd: {
      line: statementRange.end.line,
      character: statementRange.end.character - 1,
    },
    specifierRange,
    statementRange,
  }),
});

test("stateForHoverPosition returns the import whose specifier range contains the hover", () => {
  const first = state("react", {
    start: { line: 2, character: 0 },
    end: { line: 2, character: 26 },
  }, {
    start: { line: 2, character: 8 },
    end: { line: 2, character: 13 },
  });
  const second = state("date-fns", {
    start: { line: 5, character: 0 },
    end: { line: 7, character: 18 },
  }, {
    start: { line: 6, character: 2 },
    end: { line: 6, character: 10 },
  });

  assert.equal(stateForHoverPosition([first, second], { line: 2, character: 8 })?.detected.specifier, "react");
  assert.equal(stateForHoverPosition([first, second], { line: 6, character: 4 })?.detected.specifier, "date-fns");
});

test("stateForHoverPosition ignores positions outside tracked import specifier ranges", () => {
  const tracked = state("lodash-es", {
    start: { line: 3, character: 4 },
    end: { line: 3, character: 32 },
  }, {
    start: { line: 3, character: 12 },
    end: { line: 3, character: 21 },
  });

  assert.equal(stateForHoverPosition([tracked], { line: 3, character: 3 }), undefined);
  assert.equal(stateForHoverPosition([tracked], { line: 3, character: 32 }), undefined);
  assert.equal(stateForHoverPosition([tracked], { line: 3, character: 33 }), undefined);
  assert.equal(stateForHoverPosition([tracked], { line: 4, character: 0 }), undefined);
});
