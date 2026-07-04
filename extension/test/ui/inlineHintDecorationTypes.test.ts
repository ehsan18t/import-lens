import assert from "node:assert/strict";
import test from "node:test";
import {
  inlineHintDecorationLayerBuckets,
  INLINE_HINT_DECORATION_SLOTS,
  slotForSegmentIndex,
} from "../../src/ui/inlineHintDecorationLayerBuilder.js";
import type { InlineHintSegment } from "../../src/ui/inlineHintSegments.js";

const segment = (contentText: string): InlineHintSegment => ({
  contentText,
  tone: "neutral",
  themeColorId: "editorCodeLens.foreground",
  fontStyle: "italic",
  fontWeight: "400",
});

test("slotForSegmentIndex maps ordered segments to fixed slots", () => {
  assert.equal(slotForSegmentIndex(0), "primary");
  assert.equal(slotForSegmentIndex(1), "suffix0");
  assert.equal(slotForSegmentIndex(2), "suffix1");
  assert.equal(slotForSegmentIndex(3), "suffix2");
  assert.equal(slotForSegmentIndex(4), "suffix3");
  assert.equal(slotForSegmentIndex(5), undefined);
});

test("inlineHintDecorationLayerBuckets preserves segment order in slots", () => {
  const buckets = inlineHintDecorationLayerBuckets([
    segment(" 1.5 kB br"),
    segment(" · CJS"),
    segment(" · +1.5 kB br"),
  ]);

  assert.equal(buckets.primary.length, 1);
  assert.equal(buckets.suffix0.length, 1);
  assert.equal(buckets.suffix1.length, 1);
  assert.equal(buckets.suffix2.length, 0);
  assert.equal(buckets.primary[0]?.contentText, " 1.5 kB br");
  assert.equal(buckets.suffix0[0]?.contentText, " · CJS");
});

test("INLINE_HINT_DECORATION_SLOTS applies primary before suffix layers", () => {
  assert.deepEqual(INLINE_HINT_DECORATION_SLOTS, [
    "primary",
    "suffix0",
    "suffix1",
    "suffix2",
    "suffix3",
  ]);
});
