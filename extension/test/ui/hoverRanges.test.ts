import assert from "node:assert/strict";
import test from "node:test";
import type { ImportAnalysisState } from "../../src/analysis/state.js";
import { stateForHoverPosition } from "../../src/ui/hoverRanges.js";
import type { SourceRange } from "../../src/imports/types.js";

const state = (specifier: string, statementRange: SourceRange): ImportAnalysisState => ({
  status: "loading",
  detected: {
    specifier,
    packageName: specifier,
    named: [],
    importKind: "default",
    syntax: "static",
    runtime: "component",
    line: statementRange.start.line,
    quoteEnd: {
      line: statementRange.end.line,
      character: statementRange.end.character - 1,
    },
    statementRange,
  },
});

test("stateForHoverPosition returns the import whose statement range contains the hover", () => {
  const first = state("react", {
    start: { line: 2, character: 0 },
    end: { line: 2, character: 26 },
  });
  const second = state("date-fns", {
    start: { line: 5, character: 0 },
    end: { line: 7, character: 18 },
  });

  assert.equal(stateForHoverPosition([first, second], { line: 2, character: 8 })?.detected.specifier, "react");
  assert.equal(stateForHoverPosition([first, second], { line: 6, character: 4 })?.detected.specifier, "date-fns");
});

test("stateForHoverPosition ignores positions outside tracked import statement ranges", () => {
  const tracked = state("lodash-es", {
    start: { line: 3, character: 4 },
    end: { line: 3, character: 32 },
  });

  assert.equal(stateForHoverPosition([tracked], { line: 3, character: 3 }), undefined);
  assert.equal(stateForHoverPosition([tracked], { line: 3, character: 32 }), undefined);
  assert.equal(stateForHoverPosition([tracked], { line: 3, character: 33 }), undefined);
  assert.equal(stateForHoverPosition([tracked], { line: 4, character: 0 }), undefined);
});
