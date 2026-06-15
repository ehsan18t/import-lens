import * as vscode from "vscode";
import type { ImportLensConfig } from "../config.js";
import type { PackageJsonHintParts } from "./packageJsonLabels.js";
import { inlineHintDecorationGroups } from "./inlineHintDecorations.js";
import {
  packageJsonHintSegments,
  packageJsonSectionSummarySegment,
} from "./packageJsonHintSegments.js";

export { packageJsonHintDisplayText } from "./packageJsonHintSegments.js";

export interface PackageJsonHintDecorationGroups {
  readonly primary: vscode.DecorationOptions[];
  readonly suffix: vscode.DecorationOptions[];
}

export const packageJsonHintDecorationGroups = (
  parts: PackageJsonHintParts,
  anchor: vscode.Position,
  config: ImportLensConfig,
  hoverMessage?: vscode.MarkdownString,
): PackageJsonHintDecorationGroups =>
  inlineHintDecorationGroups(packageJsonHintSegments(parts, config), anchor, hoverMessage);

export const packageJsonSectionSummaryDecorationOptions = (
  label: string,
  anchor: vscode.Position,
  hoverMessage: vscode.MarkdownString,
): vscode.DecorationOptions => {
  const groups = inlineHintDecorationGroups(
    [packageJsonSectionSummarySegment(label)],
    anchor,
    hoverMessage,
  );

  return groups.primary[0]!;
};
