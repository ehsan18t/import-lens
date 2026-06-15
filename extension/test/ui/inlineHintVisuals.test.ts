import assert from "node:assert/strict";
import test from "node:test";
import { inlineHintThemeColorId } from "../../src/ui/inlineHintVisuals.js";

test("inlineHintThemeColorId uses muted editorCodeLens and gitDecoration tokens", () => {
  assert.equal(inlineHintThemeColorId("size"), "gitDecoration.addedResourceForeground");
  assert.equal(inlineHintThemeColorId("sizeMedium"), "gitDecoration.modifiedResourceForeground");
  assert.equal(inlineHintThemeColorId("sizeLow"), "gitDecoration.deletedResourceForeground");
  assert.equal(inlineHintThemeColorId("neutral"), "editorCodeLens.foreground");
  assert.equal(inlineHintThemeColorId("tag"), "descriptionForeground");
  assert.equal(inlineHintThemeColorId("info"), "gitDecoration.addedResourceForeground");
  assert.equal(inlineHintThemeColorId("action"), "gitDecoration.modifiedResourceForeground");
  assert.equal(inlineHintThemeColorId("delta"), "gitDecoration.modifiedResourceForeground");
  assert.equal(inlineHintThemeColorId("caution"), "gitDecoration.modifiedResourceForeground");
  assert.equal(inlineHintThemeColorId("alert"), "list.errorForeground");
});

test("inlineHintThemeColorId avoids loud semantic icon tokens", () => {
  const tokens = [
    inlineHintThemeColorId("size"),
    inlineHintThemeColorId("sizeLow"),
    inlineHintThemeColorId("tag"),
    inlineHintThemeColorId("info"),
    inlineHintThemeColorId("alert"),
  ];

  for (const token of tokens) {
    assert.notEqual(token, "testing.iconPassed");
    assert.notEqual(token, "editorGhostText.foreground");
    assert.notEqual(token, "problemsWarningIcon.foreground");
  }
});
