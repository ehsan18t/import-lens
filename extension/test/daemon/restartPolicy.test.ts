import assert from "node:assert/strict";
import test from "node:test";
import { restartDelayMs, shouldEnterCrashDegradedMode } from "../../src/daemon/restartPolicy.js";

test("restartDelayMs follows one second exponential backoff capped at thirty seconds", () => {
  assert.equal(restartDelayMs(1), 1000);
  assert.equal(restartDelayMs(2), 2000);
  assert.equal(restartDelayMs(3), 4000);
  assert.equal(restartDelayMs(4), 8000);
  assert.equal(restartDelayMs(10), 30000);
});

test("shouldEnterCrashDegradedMode trips after three crashes inside sixty seconds", () => {
  const now = 120_000;

  assert.equal(shouldEnterCrashDegradedMode([now - 59_000, now - 1000, now], now), true);
  assert.equal(shouldEnterCrashDegradedMode([now - 61_000, now - 1000, now], now), false);
});
