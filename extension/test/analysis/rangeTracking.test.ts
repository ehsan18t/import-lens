import assert from "node:assert/strict";
import test from "node:test";
import { DocumentAnalysisStates } from "../../src/analysis/documentStates.js";
import {
  DocumentEditLog,
  type SourceEdit,
  shiftDetectedImport,
  shiftLines,
} from "../../src/analysis/rangeTracking.js";
import type { DetectedImport } from "../../src/ipc/protocol.js";

const at = (line: number, character: number) => ({ line, character });
const edit = (start: [number, number], end: [number, number], text: string): SourceEdit => ({
  range: { start: at(...start), end: at(...end) },
  text,
});

// `import a from "a";` on `line`, 18 characters long, specifier "a" at 14..17.
const importOnLine = (line: number): DetectedImport => ({
  specifier: "a",
  packageName: "a",
  named: [],
  importKind: "default",
  syntax: "static",
  runtime: "component",
  line,
  quoteEnd: at(line, 17),
  specifierRange: { start: at(line, 14), end: at(line, 17) },
  statementRange: { start: at(line, 0), end: at(line, 18) },
});

test("a line typed above an import moves it down, hint anchor included", () => {
  const shifted = shiftDetectedImport(importOnLine(1), [edit([0, 0], [0, 0], "\n")]);

  assert.equal(shifted?.line, 2);
  assert.deepEqual(shifted?.statementRange, { start: at(2, 0), end: at(2, 18) });
  assert.deepEqual(shifted?.quoteEnd, at(2, 17));
});

test("text typed after the statement's semicolon does not grow the statement", () => {
  const shifted = shiftDetectedImport(importOnLine(0), [edit([0, 18], [0, 18], " // note")]);

  assert.deepEqual(shifted?.statementRange, { start: at(0, 0), end: at(0, 18) });
});

test("text typed in front of the statement moves its start and end along the line", () => {
  const shifted = shiftDetectedImport(importOnLine(0), [edit([0, 0], [0, 0], "  ")]);

  assert.deepEqual(shifted?.statementRange, { start: at(0, 2), end: at(0, 20) });
  assert.deepEqual(shifted?.specifierRange, { start: at(0, 16), end: at(0, 19) });
});

test("an edit inside the statement keeps it and stretches its end", () => {
  // `import a from "a";` -> `import a, { b } from "a";`
  const shifted = shiftDetectedImport(importOnLine(0), [edit([0, 8], [0, 8], ", { b }")]);

  assert.deepEqual(shifted?.statementRange, { start: at(0, 0), end: at(0, 25) });
});

test("deleting the statement's lines drops the import", () => {
  assert.equal(shiftDetectedImport(importOnLine(1), [edit([1, 0], [2, 0], "")]), null);
});

test("deleting lines above an import moves it up, in sequence with later edits", () => {
  const shifted = shiftDetectedImport(importOnLine(5), [
    edit([0, 0], [2, 0], ""),
    edit([0, 0], [0, 0], "x\ny\nz\n"),
  ]);

  assert.equal(shifted?.line, 6);
});

test("git-changed lines renumber with the edits, and a deleted line drops out", () => {
  const shifted = shiftLines(new Set([1, 3, 5]), [
    edit([0, 0], [0, 0], "new\n"),
    edit([4, 0], [5, 0], ""),
  ]);

  assert.deepEqual(
    [...shifted].sort((left, right) => left - right),
    [2, 5],
  );
});

test("the edit log hands back what changed after a version, and admits when it no longer knows", () => {
  const log = new DocumentEditLog();
  const typed = edit([0, 0], [0, 0], "x");

  log.record("doc", 2, [typed]);
  log.record("doc", 3, [typed, typed]);
  assert.equal(log.since("doc", 1)?.length, 3);
  assert.equal(log.since("doc", 2)?.length, 2);

  log.prune("doc", 3);
  assert.deepEqual(log.since("doc", 3), []);

  for (let version = 4; version <= 1004; version += 1) {
    log.record("doc", version, [typed]);
  }
  assert.equal(log.since("doc", 3), null);
  assert.equal(log.since("doc", 1000)?.length, 4);
});

test("stored states follow an edit, and a deleted import leaves the store", () => {
  const states = new DocumentAnalysisStates();
  states.set(
    "doc",
    [
      { detected: importOnLine(0), status: "loading" },
      { detected: { ...importOnLine(1), specifier: "b" }, status: "loading" },
    ],
    1,
  );

  assert.equal(states.applyEdits("doc", [edit([0, 0], [0, 0], "\n")]), false);
  assert.deepEqual(
    states.get("doc").map((state) => [state.detected.specifier, state.detected.line]),
    [
      ["a", 1],
      ["b", 2],
    ],
  );

  assert.equal(states.applyEdits("doc", [edit([1, 0], [2, 0], "")]), true);
  assert.deepEqual(
    states.get("doc").map((state) => [state.detected.specifier, state.detected.line]),
    [["b", 1]],
  );
});
