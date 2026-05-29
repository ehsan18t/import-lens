import assert from "node:assert/strict";
import test from "node:test";
import { platformTargetFrom } from "../../src/daemon/platform.js";

test("platformTargetFrom maps Windows x64 and arm64 targets", () => {
  assert.equal(platformTargetFrom("win32", "x64"), "win32-x64");
  assert.equal(platformTargetFrom("win32", "arm64"), "win32-arm64");
});

test("platformTargetFrom returns null for unsupported runtime pairs", () => {
  assert.equal(platformTargetFrom("aix", "x64"), null);
  assert.equal(platformTargetFrom("win32", "ia32"), null);
});

