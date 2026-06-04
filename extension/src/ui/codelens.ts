import * as vscode from "vscode";
import { insightLabelSuffix } from "../analysis/insights.js";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowCodeLens } from "./displayGuards.js";
import { formatImportSize } from "./format.js";

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

    return this.#store
      .get(document.uri)
      .filter((state) => state.status === "ready" && Boolean(state.result))
      .map((state) => {
        const line = Math.max(0, state.detected.line);
        const range = new vscode.Range(line, 0, line, 0);
        const result = state.result!;

        return new vscode.CodeLens(range, {
          title: `${formatImportSize(result, config, state.detected.runtime)}${insightLabelSuffix(state.insights)}`,
          command: "importLens.showImportDetails",
          arguments: [result, state.detected.runtime],
        });
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
