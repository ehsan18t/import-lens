import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = () =>
  readFileSync(new URL("../../.github/workflows/ci.yml", import.meta.url), "utf8");

test("CI runs on push to main and pull requests", () => {
  const source = workflow();

  assert.match(source, /push:\s*\n\s*branches:\s*\n\s*- main/u);
  assert.match(source, /pull_request:/u);
  assert.match(source, /cancel-in-progress: true/u);
});

test("CI delegates to the reusable validate workflow and runs accuracy", () => {
  const source = workflow();

  assert.match(source, /uses: \.\/\.github\/workflows\/validate\.yml/u);
  assert.match(source, /run_accuracy: true/u);

  // The validation steps live in the reusable workflow now, not inline here.
  assert.doesNotMatch(source, /rustup/u);
  assert.doesNotMatch(source, /pnpm test/u);
});
