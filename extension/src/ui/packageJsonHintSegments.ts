import type { ImportLensConfig } from "../config.js";
import type { PackageJsonHintParts } from "./packageJsonLabels.js";
import {
  primaryToneThemeColor,
  suffixToneThemeColor,
} from "./packageJsonHintVisuals.js";

export interface PackageJsonHintSegment {
  readonly contentText: string;
  readonly themeColorId: string;
  readonly fontStyle?: string;
  readonly margin?: string;
}

export const packageJsonHintSegments = (
  parts: PackageJsonHintParts,
  config: ImportLensConfig,
): PackageJsonHintSegment[] => {
  const segments: PackageJsonHintSegment[] = [
    {
      contentText: ` ${parts.primary}`,
      themeColorId: primaryToneThemeColor(parts.primaryTone),
      fontStyle: parts.primary === "checking..." ? "italic" : "normal",
      margin: "0 0 0 0.75rem",
    },
  ];

  if (!config.enableRegistryHints || !parts.suffix || !parts.suffixTone) {
    return segments;
  }

  segments.push({
    contentText: ` · ${parts.suffix}`,
    themeColorId: suffixToneThemeColor(parts.suffixTone),
    fontStyle: "italic",
  });

  return segments;
};

export const packageJsonHintDisplayText = (
  parts: PackageJsonHintParts,
  config: ImportLensConfig,
): string => packageJsonHintSegments(parts, config).map((segment) => segment.contentText).join("");

export const packageJsonSectionSummarySegment = (
  label: string,
): PackageJsonHintSegment => ({
  contentText: ` ${label}`,
  themeColorId: "descriptionForeground",
  fontStyle: label.includes("checking") ? "italic" : "normal",
  margin: "0 0 0 0.75rem",
});
