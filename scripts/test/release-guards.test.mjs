import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

// Guards, not echoes. Each asserts that an anti-pattern is absent, so it fails
// when a future author reintroduces the pattern -- not when this workflow is
// edited. A positive literal ("the file contains `gh release create`") could
// only fail on an edit its author already knew they were making.
//
// All four encode regressions fixed in 69828fa. None of them asserts a version:
// per the Testing Policy in CLAUDE.md, oxc is the only dependency whose version
// any test may pin.

const releaseWorkflow = readFileSync(
  new URL("../../.github/workflows/release.yml", import.meta.url),
  "utf8",
);

test("publish tokens never reach the store CLIs through argv", () => {
  // A --pat argument lands the token in the process table on the runner.
  // vsce and ovsx both read VSCE_PAT / OVSX_PAT from the environment instead.
  assert.doesNotMatch(releaseWorkflow, /--pat\b/u);
});

test("store publishing goes through publish-vsix.mjs, never a bare CLI call", () => {
  // publish-vsix.mjs passes --skip-duplicate and retries transient failures.
  // A direct `pnpm exec vsce publish` strands a partially published release.
  assert.doesNotMatch(releaseWorkflow, /pnpm exec (?:vsce|ovsx) publish/u);
});

test("VSIX publishing never loops in shell, which aborts on the first failure", () => {
  // A shell loop under `set -e` leaves every target behind the failing one
  // unpublished, with no summary of what did and did not ship.
  assert.doesNotMatch(releaseWorkflow, /for file in dist\/vsix/u);
});

test("the build run is correlated by artifact, never by scraping gh run list", () => {
  // `gh run list` matches on run-name, which is not unique per version.
  assert.doesNotMatch(releaseWorkflow, /gh run list/u);
});
