import assert from "node:assert/strict";
import test from "node:test";
import { changedLinesFromGitDiff } from "../../src/analysis/gitDiff.js";

test("changedLinesFromGitDiff returns added and modified new-file lines", () => {
  const diff = [
    "diff --git a/src/app.ts b/src/app.ts",
    "@@ -1,0 +2,2 @@",
    "+import { z } from 'zod';",
    "+const schema = z.string();",
    "@@ -8,1 +10,1 @@",
    "-import old from 'old-lib';",
    "+import next from 'next-lib';",
  ].join("\n");

  assert.deepEqual([...changedLinesFromGitDiff(diff)], [1, 2, 9]);
});

test("changedLinesFromGitDiff ignores deleted-only hunks", () => {
  const diff = [
    "@@ -5,2 +4,0 @@",
    "-import old from 'old-lib';",
    "-old();",
  ].join("\n");

  assert.deepEqual([...changedLinesFromGitDiff(diff)], []);
});
