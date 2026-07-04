import assert from "node:assert/strict";
import test from "node:test";
import { AnalyzedContentTracker } from "../../src/guidance/analyzedContentTracker.js";

test("reports unchanged only for the exact recorded text", () => {
  const tracker = new AnalyzedContentTracker();
  const key = "file:///p/package.json";

  assert.equal(tracker.isUnchanged(key, "a"), false);
  tracker.record(key, "a");
  assert.equal(tracker.isUnchanged(key, "a"), true);
  assert.equal(tracker.isUnchanged(key, "a "), false);
});

test("forget clears the recorded text so the next analyze runs", () => {
  const tracker = new AnalyzedContentTracker();
  const key = "file:///p/package.json";
  tracker.record(key, "a");
  tracker.forget(key);
  assert.equal(tracker.isUnchanged(key, "a"), false);
});

test("forgetAll clears every recorded document, including background tabs", () => {
  const tracker = new AnalyzedContentTracker();
  const visible = "file:///p/a/package.json";
  const background = "file:///p/b/package.json";
  tracker.record(visible, "a");
  tracker.record(background, "b");

  tracker.forgetAll();

  assert.equal(tracker.isUnchanged(visible, "a"), false);
  assert.equal(tracker.isUnchanged(background, "b"), false);
});
