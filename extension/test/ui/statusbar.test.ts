import assert from "node:assert/strict";
import test from "node:test";
import { statusBarText, statusBarTooltip } from "../../src/ui/statusbarText.js";

test("statusBarText prefixes with IL and shows the size for a sized state", () => {
  assert.equal(statusBarText({ kind: "size", label: "12.3 kB gzip" }), "IL: 12.3 kB gzip");
});

test("statusBarText shows Ready when idle", () => {
  assert.equal(statusBarText({ kind: "ready" }), "IL: Ready");
});

test("statusBarText shows Computing while in flight", () => {
  assert.equal(statusBarText({ kind: "computing" }), "IL: Computing…");
});

test("statusBarText shows Unavailable on daemon/error", () => {
  assert.equal(statusBarText({ kind: "unavailable" }), "IL: Unavailable");
});

test("statusBarTooltip uses colon-based app copy", () => {
  assert.equal(
    statusBarTooltip({ kind: "size", label: "12.3 kB gzip" }),
    "ImportLens: Current file bundle size (12.3 kB gzip)",
  );
  assert.equal(statusBarTooltip({ kind: "ready" }), "ImportLens: Ready");
  assert.equal(statusBarTooltip({ kind: "computing" }), "ImportLens: Computing current file size");
  assert.equal(statusBarTooltip({ kind: "unavailable" }), "ImportLens: Daemon unavailable");
});
