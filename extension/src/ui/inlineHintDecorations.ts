import * as vscode from "vscode";
import type { InlineHintSegment } from "./inlineHintSegments.js";
import {
  emptyInlineHintDecorationLayers,
  inlineHintDecorationLayers,
  type InlineHintDecorationLayers,
} from "./inlineHintDecorationTypes.js";

export interface InlineHintDecorationGroups {
  readonly primary: vscode.DecorationOptions[];
  readonly suffix: vscode.DecorationOptions[];
}

export const inlineHintDecorationGroups = (
  segments: readonly InlineHintSegment[],
  anchor: vscode.Position,
  hoverMessage?: vscode.MarkdownString,
): InlineHintDecorationGroups => {
  const layers = inlineHintDecorationLayers(segments, anchor, hoverMessage);

  return {
    primary: layers.primary,
    suffix: [...layers.suffix0, ...layers.suffix1, ...layers.suffix2, ...layers.suffix3],
  };
};

export const inlineHintDecorationLayersFromSegments = (
  segments: readonly InlineHintSegment[],
  anchor: vscode.Position,
  hoverMessage?: vscode.MarkdownString,
): InlineHintDecorationLayers => inlineHintDecorationLayers(segments, anchor, hoverMessage);

export { emptyInlineHintDecorationLayers };
