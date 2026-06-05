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
