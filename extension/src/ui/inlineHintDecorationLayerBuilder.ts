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

    if (slot) {
      buckets[slot].push(segment);
      continue;
    }

    // No fixed slot remains: fold the overflow segment's text into the last
    // suffix slot so no tag is silently dropped. The native inlay and CodeLens
    // renderers show every suffix, and all renderers must agree.
    const lastSlot = buckets.suffix3;
    const last = lastSlot[lastSlot.length - 1];
    if (last) {
      lastSlot[lastSlot.length - 1] = {
        ...last,
        contentText: `${last.contentText}${segment.contentText}`,
      };
    } else {
      lastSlot.push(segment);
    }
  }

  return buckets;
};
