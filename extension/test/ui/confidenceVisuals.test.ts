import assert from "node:assert/strict";
import test from "node:test";
import { confidenceVisualFor } from "../../src/ui/confidenceVisuals.js";

test("confidenceVisualFor provides one shared confidence color mapping", () => {
  assert.deepEqual(
    [
      confidenceVisualFor("high").themeColor,
      confidenceVisualFor("medium").themeColor,
      confidenceVisualFor("low").themeColor,
    ],
    ["charts.green", "charts.yellow", "charts.red"],
  );
  assert.equal(confidenceVisualFor("high").cssClass, "confidence-high");
  assert.equal(confidenceVisualFor("medium").label, "Medium confidence");
  assert.equal(confidenceVisualFor("low").badge, "Low");
});

test("confidenceVisualFor keeps unknown confidence visually neutral", () => {
  assert.equal(confidenceVisualFor("unknown").themeColor, "descriptionForeground");
});

test("confidenceVisualFor degrades a level from a newer daemon instead of throwing", () => {
  // An older extension meets a newer daemon routinely. The lookup used to be unguarded, so a fourth
  // ConfidenceLevel returned undefined and the caller's `.badge` read threw — taking the entire
  // hover with it rather than losing one badge.
  const visual = confidenceVisualFor("very_low" as never);
  assert.equal(visual.badge, "Unknown");
  assert.equal(visual.themeColor, "descriptionForeground");
});
