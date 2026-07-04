import type { InlineHintTone } from "./inlineHintVisuals.js";
import { inlineHintThemeColorId, inlineHintVisualFor } from "./inlineHintVisuals.js";

export interface InlineHintSegment {
  readonly contentText: string;
  readonly tone: InlineHintTone;
  readonly themeColorId: string;
  readonly fontStyle: string;
  readonly fontWeight: string;
  readonly margin?: string;
}

export interface InlineHintSuffixPart {
  readonly text: string;
  readonly tone: InlineHintTone;
}

export interface InlineHintParts {
  readonly primary: string;
  readonly primaryTone: InlineHintTone;
  readonly suffixes: readonly InlineHintSuffixPart[];
}

export interface InlineHintSegmentOptions {
  readonly primaryMargin?: string;
  readonly primaryItalic?: boolean;
}

const segmentFromTone = (
  contentText: string,
  tone: InlineHintTone,
  options?: { margin?: string; italic?: boolean },
): InlineHintSegment => {
  const visual = inlineHintVisualFor(tone);
  return {
    contentText,
    tone,
    themeColorId: inlineHintThemeColorId(tone),
    fontStyle: options?.italic === false ? "normal" : visual.fontStyle,
    fontWeight: visual.fontWeight,
    ...(options?.margin ? { margin: options.margin } : {}),
  };
};

export const inlineHintSegmentsFromParts = (
  parts: InlineHintParts,
  options: InlineHintSegmentOptions = {},
): InlineHintSegment[] => {
  const segments: InlineHintSegment[] = [
    segmentFromTone(` ${parts.primary}`, parts.primaryTone, {
      margin: options.primaryMargin ?? "0 0 0 0.75rem",
      italic: options.primaryItalic,
    }),
  ];

  for (const suffix of parts.suffixes) {
    segments.push(segmentFromTone(` · ${suffix.text}`, suffix.tone));
  }

  return segments;
};

export const inlineHintDisplayText = (parts: InlineHintParts): string =>
  [parts.primary, ...parts.suffixes.map((suffix) => suffix.text)]
    .filter((part) => part.length > 0)
    .join(" · ");
