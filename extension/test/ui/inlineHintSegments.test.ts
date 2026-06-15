import assert from "node:assert/strict";
import test from "node:test";
import {
  inlineHintDisplayText,
  inlineHintSegmentsFromParts,
} from "../../src/ui/inlineHintSegments.js";

test("inlineHintSegmentsFromParts builds primary and suffix segments", () => {
  assert.deepEqual(
    inlineHintSegmentsFromParts({
      primary: "1.5 kB br",
      primaryTone: "size",
      suffixes: [
        { text: "CJS", tone: "tag" },
        { text: "+1.5 kB br", tone: "delta" },
      ],
    }),
    [
      {
        contentText: " 1.5 kB br",
        tone: "size",
        themeColorId: "gitDecoration.addedResourceForeground",
        fontStyle: "italic",
        fontWeight: "400",
        margin: "0 0 0 0.75rem",
      },
      {
        contentText: " · CJS",
        tone: "tag",
        themeColorId: "descriptionForeground",
        fontStyle: "italic",
        fontWeight: "400",
      },
      {
        contentText: " · +1.5 kB br",
        tone: "delta",
        themeColorId: "gitDecoration.modifiedResourceForeground",
        fontStyle: "italic",
        fontWeight: "400",
      },
    ],
  );
});

test("inlineHintDisplayText joins primary and suffix labels", () => {
  assert.equal(
    inlineHintDisplayText({
      primary: "1.5 kB br",
      primaryTone: "size",
      suffixes: [{ text: "barrel", tone: "caution" }],
    }),
    "1.5 kB br · barrel",
  );
});
