import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
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

    const hints: vscode.InlayHint[] = [];

    for (const state of this.#store.get(document.uri)) {
      const position = new vscode.Position(state.detected.quoteEnd.line, state.detected.quoteEnd.character);
      let label: string | undefined;
      let tooltip: vscode.MarkdownString | undefined;

      if (state.status === "loading") {
        label = "…";
      } else if (state.status === "missing") {
        label = state.message ?? "Package not found";
      } else if (state.status === "unavailable") {
        label = state.message ?? "Daemon unavailable";
      } else if (state.status === "ready" && state.result) {
        label = formatImportSize(state.result, config, state.detected.runtime);
        tooltip = tooltipForResult(state.result, state.detected.runtime);
      }

      if (!label) {
        continue;
      }

      const hint = new vscode.InlayHint(position, label, undefined);
      hint.paddingLeft = true;
      hint.tooltip = tooltip;
      hints.push(hint);
    }

    return hints;
  }

  refresh(): void {
    this.#onDidChangeInlayHints.fire();
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#onDidChangeInlayHints.dispose();
  }
}
