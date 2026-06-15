import * as vscode from "vscode";
import type { AnalysisStore, ImportAnalysisState } from "../analysis/state.js";
import { getImportLensConfig, type ImportLensConfig } from "../config.js";
import {
  emptyInlineHintDecorationLayers,
  InlineHintSlotDecorationPool,
  inlineHintDecorationLayers,
  mergeInlineHintDecorationLayers,
} from "./inlineHintDecorationTypes.js";
import { inlineHintSegmentsFromParts } from "./inlineHintSegments.js";
import { importHintAnchorPosition } from "./importHintAnchor.js";
import { importHintParts } from "./importHintParts.js";
import { shouldShowDecorations } from "./displayGuards.js";
import { tooltipForAnalysisState } from "./tooltip.js";

export class DecorationController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #decorationPool: InlineHintSlotDecorationPool;
  readonly #subscription: vscode.Disposable;

  constructor(store: AnalysisStore) {
    this.#store = store;
    this.#decorationPool = new InlineHintSlotDecorationPool();
    this.#subscription = this.#store.onDidChange((uri) => this.refreshUri(uri));
  }

  refreshActiveEditor(): void {
    const editor = vscode.window.activeTextEditor;

    if (editor) {
      this.refreshEditor(editor);
    }
  }

  refreshVisibleEditors(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      this.refreshEditor(editor);
    }
  }

  refreshUri(uri: vscode.Uri): void {
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.toString() === uri.toString()) {
        this.refreshEditor(editor);
      }
    }
  }

  refreshEditor(editor: vscode.TextEditor): void {
    const config = getImportLensConfig();

    if (!shouldShowDecorations(config)) {
      this.#decorationPool.clearEditor(editor);
      return;
    }

    const layers = emptyInlineHintDecorationLayers();

    for (const state of this.#store.get(editor.document.uri)) {
      const stateLayers = this.decorationLayersForState(editor.document, state, config);

      if (!stateLayers) {
        continue;
      }

      mergeInlineHintDecorationLayers(layers, stateLayers);
    }

    this.#decorationPool.applyToEditor(editor, layers);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#decorationPool.dispose();
  }

  private decorationLayersForState(
    document: vscode.TextDocument,
    state: ImportAnalysisState,
    config: ImportLensConfig,
  ): ReturnType<typeof inlineHintDecorationLayers> | null {
    const parts = importHintParts(state, config);

    if (!parts) {
      return null;
    }

    const anchor = this.positionForState(document, state, config);
    const segments = inlineHintSegmentsFromParts(parts, {
      primaryMargin: config.display === "inlayHint" ? "0 0 0 0.35rem" : "0 0 0 0.75rem",
    });

    return inlineHintDecorationLayers(segments, anchor, tooltipForAnalysisState(state));
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
