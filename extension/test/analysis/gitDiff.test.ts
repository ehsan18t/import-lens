import assert from "node:assert/strict";
import test from "node:test";
import { changedLinesBetween } from "../../src/analysis/gitDiff.js";

const sorted = (lines: Set<number>): number[] => [...lines].sort((left, right) => left - right);

test("pure insertion marks only the inserted lines", () => {
  assert.deepEqual(sorted(changedLinesBetween("a\nb\nc\n", "a\nX\nY\nb\nc\n")), [1, 2]);
});

test("replacement marks the replacing line", () => {
  assert.deepEqual(sorted(changedLinesBetween("a\nb\nc\n", "a\nB\nc\n")), [1]);
});

test("pure deletion marks nothing", () => {
  assert.equal(changedLinesBetween("a\nb\nc\n", "a\nc\n").size, 0);
});

test("two separated edits do not mark the unchanged lines between them", () => {
  assert.deepEqual(sorted(changedLinesBetween("a\nb\nc\nd\ne\n", "A\nb\nc\nd\nE\n")), [0, 4]);
});

test("content lines starting with ++ are handled like any other line", () => {
  assert.deepEqual(sorted(changedLinesBetween("let i = 0;\n", "let i = 0;\n++i;\n")), [1]);
});

test("an interior unchanged line inside an edit region is not marked", () => {
  assert.deepEqual(sorted(changedLinesBetween("a\nX\nY\nZ\ne\n", "a\nX2\nY\nZ2\ne\n")), [1, 3]);
});

test("CRLF base against LF buffer compares by line content", () => {
  assert.equal(changedLinesBetween("a\r\nb\r\n", "a\nb\n").size, 0);
});

test("identical inputs mark nothing", () => {
  assert.equal(changedLinesBetween("a\nb\n", "a\nb\n").size, 0);
});
