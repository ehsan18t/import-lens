import * as vscode from "vscode";
import type { AnalysisStore, ImportAnalysisState } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { substitutionSuggestionsFor } from "../guidance/substitutions.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { shouldOfferNamedExportCandidates } from "./namedExportCandidatePolicy.js";
import { showNamedExportCandidatesCommand } from "./namedExportCandidates.js";
import { treeShakeActionReason } from "./treeShakeActionReason.js";

export class TreeShakeCodeActionProvider implements vscode.CodeActionProvider {
  readonly #store: AnalysisStore;

  constructor(store: AnalysisStore) {
    this.#store = store;
  }

  provideCodeActions(document: vscode.TextDocument, range: vscode.Range): vscode.CodeAction[] {
    if (!getImportLensConfig().enabled) {
      return [];
    }

    return this.#store
      .get(document.uri)
      .filter((state) => stateOverlapsRange(state, range))
      .flatMap((state) => this.actionsForState(document, state));
  }

  private actionsForState(
    document: vscode.TextDocument,
    state: ImportAnalysisState,
  ): vscode.CodeAction[] {
    if (state.status !== "ready" || !state.result) {
      return [];
    }

    const actions: vscode.CodeAction[] = [];
    const reason = treeShakeActionReason(state.result);

    if (reason) {
      const inspect = new vscode.CodeAction(
        `Inspect Import Lens tree-shaking: ${reason}`,
        vscode.CodeActionKind.Refactor,
      );
      inspect.command = {
        command: "importLens.showImportDetails",
        title: "Inspect Import Lens tree-shaking diagnostics",
        arguments: [state.result, state.detected.runtime],
      };

      const copy = new vscode.CodeAction(
        "Copy Import Lens tree-shaking diagnostics",
        vscode.CodeActionKind.Refactor,
      );
      copy.command = {
        command: copyImportDiagnosticsCommand,
        title: "Copy Import Lens tree-shaking diagnostics",
        arguments: [state.result],
      };

      actions.push(inspect, copy);
    }

    if (shouldOfferNamedExportCandidates(state)) {
      const namedExports = new vscode.CodeAction(
        "Show Import Lens named export candidates",
        vscode.CodeActionKind.QuickFix,
      );
      namedExports.command = {
        command: showNamedExportCandidatesCommand,
        title: "Show Import Lens named export candidates",
        arguments: [document.uri, state.detected],
      };
      actions.push(namedExports);
    }

    for (const suggestion of substitutionSuggestionsFor(
      state.detected.specifier,
      state.detected.packageName,
    )) {
      const action = new vscode.CodeAction(
        `Copy Import Lens alternative: ${suggestion.packageName}`,
        vscode.CodeActionKind.Refactor,
      );
      action.command = {
        command: "importLens.copySubstitutionSuggestion",
        title: "Copy Import Lens import alternative",
        arguments: [state.detected.specifier, suggestion.packageName, suggestion.reason],
      };
      actions.push(action);
    }

    return actions;
  }
}

const stateOverlapsRange = (state: ImportAnalysisState, range: vscode.Range): boolean => {
  const startLine = state.detected.statementRange.start.line;
  const endLine = state.detected.statementRange.end.line;

  return range.end.line >= startLine && range.start.line <= endLine;
};
