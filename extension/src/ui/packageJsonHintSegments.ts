import type { ImportLensConfig } from "../config.js";
import type { PackageJsonHintParts } from "./packageJsonLabels.js";
import {
  primaryToneThemeColor,
  suffixToneThemeColor,
  type PackageJsonSuffixTone,
} from "./packageJsonHintVisuals.js";
import type { InlineHintSegment } from "./inlineHintSegments.js";

export const packageJsonHintDisplayText = (
  parts: PackageJsonHintParts,
  config: ImportLensConfig,
): string => packageJsonHintSegments(parts, config).map((segment) => segment.contentText).join("");

export type PackageJsonHintSegment = InlineHintSegment;

const suffixInlineTone = (tone: PackageJsonSuffixTone): InlineHintSegment["tone"] => {
  if (tone === "latest") {
    return "info";
  }

  if (tone === "stale") {
    return "caution";
  }

  return "action";
};

export const packageJsonHintSegments = (
  parts: PackageJsonHintParts,
  config: ImportLensConfig,
): PackageJsonHintSegment[] => {
  const segments: PackageJsonHintSegment[] = [
    {
      contentText: ` ${parts.primary}`,
      tone: "neutral",
      themeColorId: primaryToneThemeColor(parts.primaryTone),
      fontStyle: parts.primary === "checking..." ? "italic" : "normal",
      fontWeight: "400",
      margin: "0 0 0 0.75rem",
    },
  ];

  if (!config.enableRegistryHints || !parts.suffix || !parts.suffixTone) {
    return segments;
  }

  segments.push({
    contentText: ` · ${parts.suffix}`,
    tone: suffixInlineTone(parts.suffixTone),
    themeColorId: suffixToneThemeColor(parts.suffixTone),
    fontStyle: "italic",
    fontWeight: "400",
  });

  return segments;
};

export const packageJsonSectionSummarySegment = (
  label: string,
): PackageJsonHintSegment => ({
  contentText: ` ${label}`,
  tone: "neutral",
  themeColorId: "descriptionForeground",
  fontStyle: label.includes("checking") ? "italic" : "normal",
  fontWeight: "400",
  margin: "0 0 0 0.75rem",
});
