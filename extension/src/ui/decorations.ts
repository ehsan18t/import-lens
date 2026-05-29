import * as vscode from "vscode";
import type { AnalysisStore, ImportAnalysisState } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { formatImportSize } from "./format.js";
import { tooltipForMessage, tooltipForResult } from "./tooltip.js";

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

  refreshUri(uri: vscode.Uri): void {
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.toString() === uri.toString()) {
        this.refreshEditor(editor);
      }
    }
  }

  refreshEditor(editor: vscode.TextEditor): void {
    const config = getImportLensConfig();

    if (config.display === "inlayHint" || config.useCodeLens) {
      editor.setDecorations(this.#decoration, []);
      return;
    }

    const decorations = this.#store
      .get(editor.document.uri)
      .map((state) => this.decorationForState(editor.document, state))
      .filter((value): value is vscode.DecorationOptions => Boolean(value));

    editor.setDecorations(this.#decoration, decorations);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#decoration.dispose();
  }

  private decorationForState(document: vscode.TextDocument, state: ImportAnalysisState): vscode.DecorationOptions | null {
    const line = document.lineAt(Math.min(state.detected.line, document.lineCount - 1));
    const position = line.range.end;
    const message = this.messageForState(state);

    if (!message) {
      return null;
    }

    return {
      range: new vscode.Range(position, position),
      hoverMessage: this.hoverForState(state),
      renderOptions: {
        after: {
          contentText: ` ${message}`,
          color: this.colorForState(state),
        },
      },
    };
  }

  private messageForState(state: ImportAnalysisState): string | null {
    if (state.status === "missing") {
      return state.message ?? "Package not found";
    }

    if (state.status === "loading") {
      return "Calculating...";
    }

    if (state.status === "ready" && state.result) {
      const config = getImportLensConfig();
      return formatImportSize(state.result, config, state.detected.runtime);
    }

    return null;
  }

  private hoverForState(state: ImportAnalysisState): vscode.MarkdownString | undefined {
    if (state.status === "missing") {
      return tooltipForMessage("ImportLens", state.message ?? "Package not found");
    }

    if (state.status === "ready" && state.result) {
      return tooltipForResult(state.result, state.detected.runtime);
    }

    return undefined;
  }

  private colorForState(state: ImportAnalysisState): vscode.ThemeColor {
    if (state.status === "missing" || state.result?.error) {
      return new vscode.ThemeColor("editorWarning.foreground");
    }

    return new vscode.ThemeColor("descriptionForeground");
  }
}
