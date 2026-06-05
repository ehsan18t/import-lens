import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import type { SourceRange } from "../imports/types.js";
import { shouldShowColoredSourceHovers } from "./displayGuards.js";
import { stateForHoverPosition } from "./hoverRanges.js";
import { tooltipForAnalysisState } from "./tooltip.js";

const vscodeRangeFromSourceRange = (range: SourceRange): vscode.Range =>
  new vscode.Range(
    new vscode.Position(range.start.line, range.start.character),
    new vscode.Position(range.end.line, range.end.character),
  );

export class ImportLensHoverProvider implements vscode.HoverProvider {
  readonly #store: AnalysisStore;

  constructor(store: AnalysisStore) {
    this.#store = store;
  }

  provideHover(
    document: vscode.TextDocument,
    position: vscode.Position,
    _token: vscode.CancellationToken,
  ): vscode.Hover | undefined {
    if (!shouldShowColoredSourceHovers(getImportLensConfig())) {
      return undefined;
    }

    const state = stateForHoverPosition(this.#store.get(document.uri), {
      line: position.line,
      character: position.character,
    });

    if (!state) {
      return undefined;
    }

    const tooltip = tooltipForAnalysisState(state);

    if (!tooltip) {
      return undefined;
    }

    return new vscode.Hover(tooltip, vscodeRangeFromSourceRange(state.detected.specifierRange));
  }
}
