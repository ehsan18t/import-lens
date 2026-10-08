import * as vscode from "vscode";
import type { AnalysisStore, ImportAnalysisState } from "../analysis/state.js";
import { getImportLensConfig, type ImportLensConfig } from "../config.js";
import { shouldShowDecorations } from "./displayGuards.js";
import { importHintAnchorPosition } from "./importHintAnchor.js";
import { importHintParts } from "./importHintParts.js";
import { InlineHintDecorationController } from "./inlineHintDecorationController.js";
import {
  type AnchoredInlineHint,
  inlineHintDecorationLayers,
  inlineHintLanes,
} from "./inlineHintDecorationTypes.js";
import { inlineHintSegmentsFromParts } from "./inlineHintSegments.js";
import { tooltipForAnalysisState } from "./tooltip.js";

export class DecorationController extends InlineHintDecorationController {
  readonly #store: AnalysisStore;

  constructor(store: AnalysisStore) {
    super(store);
    this.#store = store;
  }

  refreshActiveEditor(): void {
    const editor = vscode.window.activeTextEditor;

    if (editor) {
      this.refreshEditor(editor);
    }
  }

  refreshEditor(editor: vscode.TextEditor): void {
    const config = getImportLensConfig();

    if (!shouldShowDecorations(config)) {
      this.decorationPool.clearEditor(editor);
      return;
    }

    const hints = this.#store
      .get(editor.document.uri)
      .flatMap((state) => this.hintForState(editor.document, state, config) ?? []);

    this.decorationPool.applyToEditor(editor, inlineHintLanes(hints));
  }

  private hintForState(
    document: vscode.TextDocument,
    state: ImportAnalysisState,
    config: ImportLensConfig,
  ): AnchoredInlineHint | null {
    const parts = importHintParts(state, config);

    if (!parts) {
      return null;
    }

    const anchor = this.positionForState(document, state, config);
    const segments = inlineHintSegmentsFromParts(parts, {
      primaryMargin: config.display === "inlayHint" ? "0 0 0 0.35rem" : "0 0 0 0.75rem",
    });

    return {
      anchor,
      layers: inlineHintDecorationLayers(segments, anchor, tooltipForAnalysisState(state)),
    };
  }

  private positionForState(
    document: vscode.TextDocument,
    state: ImportAnalysisState,
    config: ImportLensConfig,
  ): vscode.Position {
    if (config.display === "inlayHint") {
      const position = importHintAnchorPosition(document, state.detected);
      return new vscode.Position(position.line, position.character);
    }

    const line = document.lineAt(Math.min(state.detected.line, document.lineCount - 1));
    return line.range.end;
  }
}
