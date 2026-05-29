import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import type { ImportResult } from "../ipc/protocol.js";
import { formatImportSize } from "./format.js";
import { tooltipForResult } from "./tooltip.js";

export class ImportLensInlayHintsProvider implements vscode.InlayHintsProvider, vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #onDidChangeInlayHints = new vscode.EventEmitter<void>();
  readonly #subscription: vscode.Disposable;

  readonly onDidChangeInlayHints: vscode.Event<void> = this.#onDidChangeInlayHints.event;

  constructor(store: AnalysisStore) {
    this.#store = store;
    this.#subscription = this.#store.onDidChange(() => this.#onDidChangeInlayHints.fire());
  }

  provideInlayHints(document: vscode.TextDocument): vscode.InlayHint[] {
    const config = getImportLensConfig();

    if (config.display !== "inlayHint") {
      return [];
    }

    return this.#store
      .get(document.uri)
      .filter((state) => state.status === "ready" && Boolean(state.result))
      .map((state) => {
        const result = state.result as ImportResult;
        const hint = new vscode.InlayHint(
          new vscode.Position(state.detected.quoteEnd.line, state.detected.quoteEnd.character),
          formatImportSize(result, config),
          undefined,
        );
        hint.paddingLeft = true;
        hint.tooltip = tooltipForResult(result);
        return hint;
      });
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#onDidChangeInlayHints.dispose();
  }
}
