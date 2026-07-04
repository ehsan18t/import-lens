import * as vscode from "vscode";
import {
  INLINE_HINT_DECORATION_SLOTS,
  inlineHintDecorationLayerBuckets,
  type InlineHintDecorationSlot,
} from "./inlineHintDecorationLayerBuilder.js";
import type { InlineHintSegment } from "./inlineHintSegments.js";

export {
  INLINE_HINT_DECORATION_SLOTS,
  INLINE_HINT_SUFFIX_SLOT_COUNT,
} from "./inlineHintDecorationLayerBuilder.js";
export type { InlineHintDecorationSlot } from "./inlineHintDecorationLayerBuilder.js";

export interface InlineHintDecorationLayers {
  readonly primary: vscode.DecorationOptions[];
  readonly suffix0: vscode.DecorationOptions[];
  readonly suffix1: vscode.DecorationOptions[];
  readonly suffix2: vscode.DecorationOptions[];
  readonly suffix3: vscode.DecorationOptions[];
}

export const emptyInlineHintDecorationLayers = (): InlineHintDecorationLayers => ({
  primary: [],
  suffix0: [],
  suffix1: [],
  suffix2: [],
  suffix3: [],
});

export const decorationOptionForSegment = (
  segment: InlineHintSegment,
  anchor: vscode.Position,
  hoverMessage?: vscode.MarkdownString,
): vscode.DecorationOptions => ({
  range: new vscode.Range(anchor, anchor),
  hoverMessage,
  renderOptions: {
    after: {
      contentText: segment.contentText,
      color: new vscode.ThemeColor(segment.themeColorId),
      fontStyle: segment.fontStyle,
      fontWeight: segment.fontWeight,
      margin: segment.margin,
    },
  },
});

export const inlineHintDecorationLayers = (
  segments: readonly InlineHintSegment[],
  anchor: vscode.Position,
  hoverMessage?: vscode.MarkdownString,
): InlineHintDecorationLayers => {
  const layers: InlineHintDecorationLayers = emptyInlineHintDecorationLayers();
  const buckets = inlineHintDecorationLayerBuckets(segments);

  for (const slot of INLINE_HINT_DECORATION_SLOTS) {
    layers[slot].push(
      ...buckets[slot].map((segment, index) =>
        decorationOptionForSegment(
          segment,
          anchor,
          slot === "primary" && index === 0 ? hoverMessage : undefined,
        ),
      ),
    );
  }

  return layers;
};

export const mergeInlineHintDecorationLayers = (
  target: InlineHintDecorationLayers,
  source: InlineHintDecorationLayers,
): void => {
  for (const slot of INLINE_HINT_DECORATION_SLOTS) {
    target[slot].push(...source[slot]);
  }
};

export class InlineHintSlotDecorationPool implements vscode.Disposable {
  readonly #types: Record<InlineHintDecorationSlot, vscode.TextEditorDecorationType>;

  constructor() {
    this.#types = {
      primary: vscode.window.createTextEditorDecorationType({
        rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
      }),
      suffix0: vscode.window.createTextEditorDecorationType({
        rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
      }),
      suffix1: vscode.window.createTextEditorDecorationType({
        rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
      }),
      suffix2: vscode.window.createTextEditorDecorationType({
        rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
      }),
      suffix3: vscode.window.createTextEditorDecorationType({
        rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
      }),
    };
  }

  applyToEditor(editor: vscode.TextEditor, layers: InlineHintDecorationLayers): void {
    for (const slot of INLINE_HINT_DECORATION_SLOTS) {
      editor.setDecorations(this.#types[slot], layers[slot]);
    }
  }

  clearEditor(editor: vscode.TextEditor): void {
    for (const slot of INLINE_HINT_DECORATION_SLOTS) {
      editor.setDecorations(this.#types[slot], []);
    }
  }

  dispose(): void {
    for (const slot of INLINE_HINT_DECORATION_SLOTS) {
      this.#types[slot].dispose();
    }
  }
}
