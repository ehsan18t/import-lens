import assert from "node:assert/strict";
import test from "node:test";
import {
  primaryToneThemeColor,
  suffixToneThemeColor,
} from "../../src/ui/packageJsonHintVisuals.js";

test("primaryToneThemeColor maps unavailable to red and neutral to muted foreground", () => {
  assert.equal(primaryToneThemeColor("unavailable"), "charts.red");
  assert.equal(primaryToneThemeColor("neutral"), "descriptionForeground");
});

test("suffixToneThemeColor maps registry statuses to green and amber", () => {
  assert.equal(suffixToneThemeColor("latest"), "charts.green");
  assert.equal(suffixToneThemeColor("update"), "charts.yellow");
  assert.equal(suffixToneThemeColor("install"), "charts.yellow");
});
