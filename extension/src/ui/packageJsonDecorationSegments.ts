import * as vscode from "vscode";
import type { ImportLensConfig } from "../config.js";
import type { PackageJsonHintParts } from "./packageJsonLabels.js";
import {
  packageJsonHintSegments,
  packageJsonHintDisplayText,
  packageJsonSectionSummarySegment,
  type PackageJsonHintSegment,
} from "./packageJsonHintSegments.js";

export { packageJsonHintDisplayText };

export interface PackageJsonHintDecorationGroups {
  readonly primary: vscode.DecorationOptions[];
  readonly suffix: vscode.DecorationOptions[];
}

const decorationOptionForSegment = (
  segment: PackageJsonHintSegment,
  position: vscode.Position,
  hoverMessage?: vscode.MarkdownString,
): vscode.DecorationOptions => ({
  range: new vscode.Range(position, position),
  hoverMessage,
  renderOptions: {
    after: {
      contentText: segment.contentText,
      color: new vscode.ThemeColor(segment.themeColorId),
      fontStyle: segment.fontStyle,
      margin: segment.margin,
    },
  },
});

export const packageJsonHintDecorationGroups = (
  parts: PackageJsonHintParts,
  position: vscode.Position,
  config: ImportLensConfig,
  hoverMessage?: vscode.MarkdownString,
): PackageJsonHintDecorationGroups => {
  const segments = packageJsonHintSegments(parts, config);
  const primary = segments[0];

  if (!primary) {
    return { primary: [], suffix: [] };
  }

  return {
    primary: [decorationOptionForSegment(primary, position, hoverMessage)],
    suffix: segments.slice(1).map((segment) => decorationOptionForSegment(segment, position, hoverMessage)),
  };
};

export const packageJsonSectionSummaryDecorationOptions = (
  label: string,
  position: vscode.Position,
  hoverMessage: vscode.MarkdownString,
): vscode.DecorationOptions =>
  decorationOptionForSegment(packageJsonSectionSummarySegment(label), position, hoverMessage);
