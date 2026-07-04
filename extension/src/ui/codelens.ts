import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowCodeLens } from "./displayGuards.js";
import { importHintParts } from "./importHintParts.js";
import { inlineHintDisplayText } from "./inlineHintSegments.js";

export class ImportLensCodeLensProvider implements vscode.CodeLensProvider, vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #onDidChangeCodeLenses = new vscode.EventEmitter<void>();
  readonly #subscription: vscode.Disposable;

  readonly onDidChangeCodeLenses: vscode.Event<void> = this.#onDidChangeCodeLenses.event;

  constructor(store: AnalysisStore) {
    this.#store = store;
    this.#subscription = this.#store.onDidChange(() => this.#onDidChangeCodeLenses.fire());
  }

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const config = getImportLensConfig();

    if (!shouldShowCodeLens(config)) {
      return [];
    }

    return this.#store.get(document.uri).flatMap((state) => {
      const { result } = state;
      if (state.status !== "ready" || !result) {
        return [];
      }
      const line = Math.max(0, state.detected.line);
      const range = new vscode.Range(line, 0, line, 0);
      const parts = importHintParts(state, config);
      const title = parts ? inlineHintDisplayText(parts) : "";

      return [
        new vscode.CodeLens(range, {
          title,
          command: "importLens.showImportDetails",
          arguments: [result, state.detected.runtime],
        }),
      ];
    });
  }

  refresh(): void {
    this.#onDidChangeCodeLenses.fire();
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#onDidChangeCodeLenses.dispose();
  }
}
