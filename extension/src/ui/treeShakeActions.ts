import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import type { AnalysisStore, ImportAnalysisState } from "../analysis/state.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { shouldOfferNamedExportCandidates } from "./namedExportCandidatePolicy.js";
import { showNamedExportCandidatesCommand } from "./namedExportCandidates.js";
import { treeShakeActionReason } from "./treeShakeActionReason.js";

export class TreeShakeCodeActionProvider implements vscode.CodeActionProvider {
  readonly #store: AnalysisStore;

  constructor(store: AnalysisStore) {
    this.#store = store;
  }

  provideCodeActions(
    document: vscode.TextDocument,
    range: vscode.Range,
  ): vscode.CodeAction[] {
    if (!getImportLensConfig().enabled) {
      return [];
    }

    return this.#store
      .get(document.uri)
      .filter((state) => stateOverlapsRange(state, range))
      .flatMap((state) => this.actionsForState(document, state));
  }

  private actionsForState(document: vscode.TextDocument, state: ImportAnalysisState): vscode.CodeAction[] {
    if (state.status !== "ready" || !state.result) {
      return [];
    }

    const reason = treeShakeActionReason(state.result);
    if (!reason) {
      return [];
    }

    const inspect = new vscode.CodeAction(
      `Inspect ImportLens tree-shaking: ${reason}`,
      vscode.CodeActionKind.Refactor,
    );
    inspect.command = {
      command: "importLens.showImportDetails",
      title: "Inspect ImportLens tree-shaking diagnostics",
      arguments: [state.result, state.detected.runtime],
    };

    const copy = new vscode.CodeAction(
      "Copy ImportLens tree-shaking diagnostics",
      vscode.CodeActionKind.Refactor,
    );
    copy.command = {
      command: copyImportDiagnosticsCommand,
      title: "Copy ImportLens tree-shaking diagnostics",
      arguments: [state.result],
    };

    const actions = [inspect, copy];

    if (shouldOfferNamedExportCandidates(state)) {
      const namedExports = new vscode.CodeAction(
        "Show ImportLens named export candidates",
        vscode.CodeActionKind.QuickFix,
      );
      namedExports.command = {
        command: showNamedExportCandidatesCommand,
        title: "Show ImportLens named export candidates",
        arguments: [document.uri, state.detected],
      };
      actions.push(namedExports);
    }

    return actions;
  }
}

const stateOverlapsRange = (state: ImportAnalysisState, range: vscode.Range): boolean => {
  const startLine = state.detected.statementRange.start.line;
  const endLine = state.detected.statementRange.end.line;

  return range.end.line >= startLine && range.start.line <= endLine;
};
