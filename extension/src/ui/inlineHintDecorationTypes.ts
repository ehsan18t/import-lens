import * as vscode from "vscode";
import {
  decorationLanesForAnchors,
  INLINE_HINT_DECORATION_SLOTS,
  type InlineHintDecorationSlot,
  inlineHintDecorationLayerBuckets,
} from "./inlineHintDecorationLayerBuilder.js";
import type { InlineHintSegment } from "./inlineHintSegments.js";

export type { InlineHintDecorationSlot } from "./inlineHintDecorationLayerBuilder.js";
export {
  INLINE_HINT_DECORATION_SLOTS,
  INLINE_HINT_SUFFIX_SLOT_COUNT,
} from "./inlineHintDecorationLayerBuilder.js";

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

export interface AnchoredInlineHint {
  readonly anchor: vscode.Position;
  readonly layers: InlineHintDecorationLayers;
}

/** Hints grouped into decoration lanes, so hints that share an anchor render one after another. */
export const inlineHintLanes = (
  hints: readonly AnchoredInlineHint[],
): InlineHintDecorationLayers[] => {
  const laneIndexes = decorationLanesForAnchors(
    hints.map(({ anchor }) => `${anchor.line}:${anchor.character}`),
  );
  const lanes: InlineHintDecorationLayers[] = [];

  for (const [index, { layers }] of hints.entries()) {
    const lane = laneIndexes[index] ?? 0;
    lanes[lane] ??= emptyInlineHintDecorationLayers();
    mergeInlineHintDecorationLayers(lanes[lane], layers);
  }

  return lanes;
};

type InlineHintLaneTypes = Record<InlineHintDecorationSlot, vscode.TextEditorDecorationType>;

const createLaneTypes = (): InlineHintLaneTypes => {
  const lane: Partial<InlineHintLaneTypes> = {};

  for (const slot of INLINE_HINT_DECORATION_SLOTS) {
    lane[slot] = vscode.window.createTextEditorDecorationType({
      rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
    });
  }

  return lane as InlineHintLaneTypes;
};

/**
 * The slot decoration types, in lanes. A lane holds one hint per anchor; a second import at the same
 * anchor (`import React, { useState } from "react"` is two) goes in the next lane, which is set after
 * the previous one, so its whole hint renders after the first instead of interleaving slot by slot
 * ("45 kB br 3.2 kB br · over budget" with the suffix beside the wrong number). Lanes are created on
 * first need.
 */
export class InlineHintSlotDecorationPool implements vscode.Disposable {
  readonly #lanes: InlineHintLaneTypes[] = [];

  applyToEditor(editor: vscode.TextEditor, lanes: readonly InlineHintDecorationLayers[]): void {
    while (this.#lanes.length < lanes.length) {
      this.#lanes.push(createLaneTypes());
    }

    for (const [index, types] of this.#lanes.entries()) {
      const layers = lanes[index];

      for (const slot of INLINE_HINT_DECORATION_SLOTS) {
        editor.setDecorations(types[slot], layers ? layers[slot] : []);
      }
    }
  }

  clearEditor(editor: vscode.TextEditor): void {
    this.applyToEditor(editor, []);
  }

  dispose(): void {
    for (const types of this.#lanes) {
      for (const slot of INLINE_HINT_DECORATION_SLOTS) {
        types[slot].dispose();
      }
    }
  }
}
