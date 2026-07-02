import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const manifest = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8"));
const performanceTest = readFileSync(new URL("../../daemon/tests/performance.rs", import.meta.url), "utf8");
const accuracyCompare = readFileSync(new URL("../accuracy-compare.mjs", import.meta.url), "utf8");

test("performance smoke is explicit and runs in release mode", () => {
  assert.equal(manifest.scripts["test:rust"], "cargo test --workspace");
  assert.equal(
    manifest.scripts["test:performance"],
    "cargo test -p import-lens-daemon --release --test performance -- --ignored --nocapture",
  );
  assert.match(performanceTest, /#\[ignore = "release-only performance smoke run by pnpm test:performance"\]/);
});

test("accuracy comparator is explicit and esbuild-backed", () => {
  assert.equal(manifest.scripts["test:accuracy"], "node scripts/accuracy-compare.mjs");
  assert.equal(manifest.devDependencies.esbuild, "0.28.1");
  assert.match(accuracyCompare, /import \* as esbuild from "esbuild"/);
});
