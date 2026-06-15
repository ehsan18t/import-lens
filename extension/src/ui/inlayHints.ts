import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowNativeInlayHints } from "./displayGuards.js";
import { importHintAnchorPosition } from "./importHintAnchor.js";
import { importHintParts } from "./importHintParts.js";
import { inlineHintSegmentsFromParts } from "./inlineHintSegments.js";
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
      const parts = importHintParts(state, config);

      if (!parts) {
        continue;
      }

      const anchor = importHintAnchorPosition(document, state.detected);
      const position = new vscode.Position(anchor.line, anchor.character);
      const segments = inlineHintSegmentsFromParts(parts, {
        primaryMargin: "0 0 0 0.35rem",
      });
      const stateTooltip = tooltipForAnalysisState(state);
      const labelParts = segments.map((segment, index) => {
        const value = index === 0 ? segment.contentText.trimStart() : segment.contentText;
        const labelPart = new vscode.InlayHintLabelPart(value);
        labelPart.tooltip = stateTooltip;

        if (state.status === "ready" && state.result && !state.result.error) {
          labelPart.command = {
            title: "Show Import Details",
            command: "importLens.showImportDetails",
            arguments: [state.result, state.detected.runtime],
          };
        }

        return labelPart;
      });

      const hint = new vscode.InlayHint(position, labelParts, undefined);
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
