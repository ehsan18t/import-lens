import * as vscode from "vscode";
import { insightLabelSuffix } from "../analysis/insights.js";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowNativeInlayHints } from "./displayGuards.js";
import { formatImportSize } from "./format.js";
import { tooltipForAnalysisState } from "./tooltip.js";

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

    if (!shouldShowNativeInlayHints(config)) {
      return [];
    }

    const hints: vscode.InlayHint[] = [];

    for (const state of this.#store.get(document.uri)) {
      const position = new vscode.Position(state.detected.statementRange.end.line, state.detected.statementRange.end.character);
      let labelString: string | undefined;

      if (state.status === "loading") {
        labelString = "…";
      } else if (state.status === "missing") {
        labelString = state.message ?? "Package not found";
      } else if (state.status === "unavailable") {
        continue;
      } else if (state.status === "ready" && state.result) {
        labelString = `${formatImportSize(state.result, config, state.detected.runtime)}${insightLabelSuffix(state.insights)}`;
      }

      if (!labelString) {
        continue;
      }

      const labelPart = new vscode.InlayHintLabelPart(labelString);
      labelPart.tooltip = tooltipForAnalysisState(state);

      if (state.status === "ready" && state.result) {
        labelPart.command = {
          title: "Show Import Details",
          command: "importLens.showImportDetails",
          arguments: [state.result, state.detected.runtime],
        };
      }

      const hint = new vscode.InlayHint(position, [labelPart], undefined);
      hint.paddingLeft = true;
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
