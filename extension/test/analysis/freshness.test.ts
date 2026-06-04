import assert from "node:assert/strict";
import test from "node:test";
import { AnalysisFreshnessTracker } from "../../src/analysis/freshness.js";

test("AnalysisFreshnessTracker invalidates older requests when a document starts new analysis", () => {
  const tracker = new AnalysisFreshnessTracker();
  const first = tracker.begin("file:///app/src/main.ts");
  const second = tracker.begin("file:///app/src/main.ts");

  assert.equal(tracker.isCurrent("file:///app/src/main.ts", first), false);
  assert.equal(tracker.isCurrent("file:///app/src/main.ts", second), true);
});

test("AnalysisFreshnessTracker tracks documents independently", () => {
  const tracker = new AnalysisFreshnessTracker();
  const first = tracker.begin("file:///app/src/main.ts");
  const other = tracker.begin("file:///app/src/other.ts");

  assert.equal(tracker.isCurrent("file:///app/src/main.ts", first), true);
  assert.equal(tracker.isCurrent("file:///app/src/other.ts", other), true);
});

test("AnalysisFreshnessTracker forgets closed documents", () => {
  const tracker = new AnalysisFreshnessTracker();
  const requestId = tracker.begin("file:///app/src/main.ts");

  tracker.forget("file:///app/src/main.ts");

  assert.equal(tracker.isCurrent("file:///app/src/main.ts", requestId), false);
});
