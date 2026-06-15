import assert from "node:assert/strict";
import test from "node:test";
import {
  primaryToneThemeColor,
  suffixToneThemeColor,
} from "../../src/ui/packageJsonHintVisuals.js";

test("primaryToneThemeColor keeps package.json primaries muted", () => {
  assert.equal(primaryToneThemeColor("unavailable"), "list.errorForeground");
  assert.equal(primaryToneThemeColor("neutral"), "descriptionForeground");
});

test("suffixToneThemeColor maps registry statuses to git decoration tokens", () => {
  assert.equal(suffixToneThemeColor("latest"), "gitDecoration.addedResourceForeground");
  assert.equal(suffixToneThemeColor("update"), "gitDecoration.modifiedResourceForeground");
  assert.equal(suffixToneThemeColor("install"), "gitDecoration.modifiedResourceForeground");
});
