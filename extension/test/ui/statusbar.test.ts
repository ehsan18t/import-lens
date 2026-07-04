import assert from "node:assert/strict";
import test from "node:test";
import { statusBarText } from "../../src/ui/statusbarText.js";

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
