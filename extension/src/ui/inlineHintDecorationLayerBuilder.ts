import type { InlineHintSegment } from "./inlineHintSegments.js";

export const INLINE_HINT_SUFFIX_SLOT_COUNT = 4;

export type InlineHintDecorationSlot =
  | "primary"
  | "suffix0"
  | "suffix1"
  | "suffix2"
  | "suffix3";

export const INLINE_HINT_DECORATION_SLOTS: readonly InlineHintDecorationSlot[] = [
  "primary",
  "suffix0",
  "suffix1",
  "suffix2",
  "suffix3",
];

export type InlineHintDecorationLayerBuckets = Record<InlineHintDecorationSlot, InlineHintSegment[]>;

export const emptyInlineHintDecorationLayerBuckets = (): InlineHintDecorationLayerBuckets => ({
  primary: [],
  suffix0: [],
  suffix1: [],
  suffix2: [],
  suffix3: [],
});

export const slotForSegmentIndex = (index: number): InlineHintDecorationSlot | undefined => {
  if (index === 0) {
    return "primary";
  }

  const suffixIndex = index - 1;

  if (suffixIndex >= INLINE_HINT_SUFFIX_SLOT_COUNT) {
    return undefined;
  }

  return INLINE_HINT_DECORATION_SLOTS[suffixIndex + 1];
};

export const inlineHintDecorationLayerBuckets = (
  segments: readonly InlineHintSegment[],
): InlineHintDecorationLayerBuckets => {
  const buckets = emptyInlineHintDecorationLayerBuckets();

  for (const [index, segment] of segments.entries()) {
    const slot = slotForSegmentIndex(index);

    if (!slot) {
      continue;
    }

    buckets[slot].push(segment);
  }

  return buckets;
};
