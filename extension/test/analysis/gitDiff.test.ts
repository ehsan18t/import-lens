import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { changedLinesBetween, changedLinesForFile } from "../../src/analysis/gitDiff.js";

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

test("a byte-order mark on the committed text is not a change", () => {
  assert.equal(changedLinesBetween("﻿import a from 'a';\nb\n", "import a from 'a';\nb\n").size, 0);
});

test("changedLinesForFile diffs a nested tracked file against HEAD and ignores untracked ones", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-gitdiff-"));
  try {
    const git = (...args: string[]): void => {
      execFileSync("git", ["-C", root, "-c", "user.name=t", "-c", "user.email=t@t", ...args]);
    };
    git("init", "-q");
    await mkdir(path.join(root, "src"));
    const tracked = path.join(root, "src", "a.ts");
    const committed = "import a from 'a';\nconst x = 1;\n";
    await writeFile(tracked, committed);
    git("add", ".");
    git("commit", "-q", "-m", "init");

    assert.equal((await changedLinesForFile(tracked, committed)).size, 0);
    assert.deepEqual(
      sorted(
        await changedLinesForFile(
          tracked,
          "import a from 'a';\nimport b from 'b';\nconst x = 1;\n",
        ),
      ),
      [1],
    );
    assert.equal((await changedLinesForFile(path.join(root, "src", "new.ts"), "x\n")).size, 0);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("changedLinesForFile outside a repository reports no changed lines", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "import-lens-nogit-"));
  try {
    assert.equal((await changedLinesForFile(path.join(root, "a.ts"), "x\n")).size, 0);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
