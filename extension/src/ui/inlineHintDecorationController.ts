import * as vscode from "vscode";
import { InlineHintSlotDecorationPool } from "./inlineHintDecorationTypes.js";

export interface DecorationRefreshSource {
  readonly onDidChange: vscode.Event<vscode.Uri>;
}

/**
 * Shared lifecycle for the inline-hint decoration controllers: it owns the
 * decoration pool, subscribes to a change source, and fans refreshes out to the
 * visible editors. Subclasses only implement {@link refreshEditor}.
 */
export abstract class InlineHintDecorationController implements vscode.Disposable {
  protected readonly decorationPool: InlineHintSlotDecorationPool = new InlineHintSlotDecorationPool();
  readonly #subscription: vscode.Disposable;

  constructor(source: DecorationRefreshSource) {
    this.#subscription = source.onDidChange((uri) => this.refreshUri(uri));
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

  abstract refreshEditor(editor: vscode.TextEditor): void;

  dispose(): void {
    this.#subscription.dispose();
    this.decorationPool.dispose();
  }
}
