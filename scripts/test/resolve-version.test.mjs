import assert from "node:assert/strict";
import test from "node:test";
import { resolveVersion } from "../resolve-version.mjs";

test("resolveVersion prefers a non-empty requested version", () => {
  assert.equal(resolveVersion("0.2.0", "0.1.0"), "0.2.0");
});

test("resolveVersion falls back to the manifest version when none is requested", () => {
  assert.equal(resolveVersion("", "0.1.0"), "0.1.0");
  assert.equal(resolveVersion(undefined, "0.1.0"), "0.1.0");
});

test("resolveVersion trims a requested version and ignores whitespace-only input", () => {
  assert.equal(resolveVersion("  0.3.0  ", "0.1.0"), "0.3.0");
  assert.equal(resolveVersion("   ", "0.1.0"), "0.1.0");
});
