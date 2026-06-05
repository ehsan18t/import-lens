import * as vscode from "vscode";
import { insightLabelSuffix } from "../analysis/insights.js";
import type { AnalysisStore, ImportAnalysisState } from "../analysis/state.js";
import { getImportLensConfig, type ImportLensConfig } from "../config.js";
import { confidenceVisualFor } from "./confidenceVisuals.js";
import { shouldShowDecorations } from "./displayGuards.js";
import { formatImportSize } from "./format.js";
import { tooltipForAnalysisState } from "./tooltip.js";

export class DecorationController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #decoration: vscode.TextEditorDecorationType;
  readonly #subscription: vscode.Disposable;

  constructor(store: AnalysisStore) {
    this.#store = store;
    this.#decoration = vscode.window.createTextEditorDecorationType({
      after: {
        margin: "0 0 0 0.75rem",
        fontStyle: "italic",
      },
      rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
    });
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
      editor.setDecorations(this.#decoration, []);
      return;
    }

    const decorations = this.#store
      .get(editor.document.uri)
      .map((state) => this.decorationForState(editor.document, state, config))
      .filter((value): value is vscode.DecorationOptions => Boolean(value));

    editor.setDecorations(this.#decoration, decorations);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#decoration.dispose();
  }

  private decorationForState(
    document: vscode.TextDocument,
    state: ImportAnalysisState,
    config: ImportLensConfig,
  ): vscode.DecorationOptions | null {
    const position = this.positionForState(document, state, config);
    const message = this.messageForState(state, config);

    if (!message) {
      return null;
    }

    return {
      range: new vscode.Range(position, position),
      hoverMessage: tooltipForAnalysisState(state),
      renderOptions: {
        after: {
          contentText: ` ${message}`,
          color: this.colorForState(state),
          fontStyle: state.status === "loading" ? "italic" : "normal",
          fontWeight: this.fontWeightForState(state),
          margin: config.display === "inlayHint" ? "0 0 0 0.35rem" : "0 0 0 0.75rem",
        },
      },
    };
  }

  private positionForState(
    document: vscode.TextDocument,
    state: ImportAnalysisState,
    config: ImportLensConfig,
  ): vscode.Position {
    if (config.display === "inlayHint") {
      const lineNumber = Math.min(state.detected.quoteEnd.line, document.lineCount - 1);
      const line = document.lineAt(lineNumber);
      return new vscode.Position(lineNumber, Math.min(state.detected.quoteEnd.character, line.text.length));
    }

    const line = document.lineAt(Math.min(state.detected.line, document.lineCount - 1));
    return line.range.end;
  }

  private messageForState(state: ImportAnalysisState, config: ImportLensConfig): string | null {
    if (state.status === "missing") {
      return state.message ?? "Package not found";
    }

    if (state.status === "loading") {
      return "Calculating...";
    }

    if (state.status === "unavailable") {
      return state.message ?? "Daemon unavailable";
    }

    if (state.status === "ready" && state.result) {
      return `${formatImportSize(state.result, config, state.detected.runtime)}${insightLabelSuffix(state.insights)}`;
    }

    return null;
  }

  private colorForState(state: ImportAnalysisState): vscode.ThemeColor {
    if (state.status === "missing" || state.status === "unavailable" || state.result?.error) {
      return new vscode.ThemeColor(confidenceVisualFor("low").themeColor);
    }

    if (state.status === "ready" && state.result) {
      return new vscode.ThemeColor(confidenceVisualFor(state.result.confidence).themeColor);
    }

    return new vscode.ThemeColor("descriptionForeground");
  }

  private fontWeightForState(state: ImportAnalysisState): string {
    if (state.status === "ready" && state.result && !state.result.error) {
      return confidenceVisualFor(state.result.confidence).fontWeight;
    }

    return "400";
  }
}
