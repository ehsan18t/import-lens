import assert from "node:assert/strict";
import test from "node:test";
import {
  inlineHintDisplayText,
  inlineHintSegmentsFromParts,
} from "../../src/ui/inlineHintSegments.js";
import { inlineHintDecorationLayerBuckets } from "../../src/ui/inlineHintDecorationLayerBuilder.js";

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

test("suffixes beyond the four slots fold into the last slot instead of dropping", () => {
  const segments = inlineHintSegmentsFromParts({
    primary: "1.2 kB br",
    primaryTone: "size",
    suffixes: [
      { text: "server", tone: "tag" },
      { text: "types only", tone: "tag" },
      { text: "CJS", tone: "tag" },
      { text: "+2 kB br", tone: "delta" },
      { text: "over budget", tone: "alert" },
      { text: "barrel", tone: "caution" },
    ],
  });
  const buckets = inlineHintDecorationLayerBuckets(segments);

  const rendered = [...buckets.suffix0, ...buckets.suffix1, ...buckets.suffix2, ...buckets.suffix3]
    .map((segment) => segment.contentText)
    .join("");
  for (const label of ["server", "types only", "CJS", "+2 kB br", "over budget", "barrel"]) {
    assert.ok(rendered.includes(label), `dropped suffix: ${label}`);
  }
});
