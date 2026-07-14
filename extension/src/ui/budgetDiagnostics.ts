import * as vscode from "vscode";
import { budgetViolationsForStates } from "../analysis/budgets.js";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import { rangeFromSourceRange } from "./vscodeRanges.js";

export class BudgetDiagnosticsController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #collection: vscode.DiagnosticCollection;
  readonly #subscription: vscode.Disposable;

  constructor(store: AnalysisStore) {
    this.#store = store;
    this.#collection = vscode.languages.createDiagnosticCollection("importLens");
    this.#subscription = this.#store.onDidChange((uri) => this.refreshUri(uri));
  }

  refreshVisibleEditors(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      this.refreshUri(editor.document.uri);
    }
  }

  refreshUri(uri: vscode.Uri): void {
    const config = getImportLensConfig();

    if (!config.enabled) {
      this.#collection.delete(uri);
      return;
    }

    // The states AND the File Cost: the per-import budget is genuinely per-import, the file budget
    // is judged against the daemon's combined build over the whole file, and this is the one place
    // that holds both (`AnalysisStore.setFileCost`). With no File Cost in the store the file budget
    // is not evaluated — never re-derived from the states, which sum to a different quantity.
    const diagnostics = budgetViolationsForStates(
      this.#store.get(uri),
      config.budgets,
      this.#store.fileCost(uri),
    ).map((violation) => {
      const diagnostic = new vscode.Diagnostic(
        rangeFromSourceRange(violation.range),
        violation.message,
        vscode.DiagnosticSeverity.Warning,
      );
      diagnostic.source = "Import Lens";
      diagnostic.code = violation.kind === "file" ? "file-budget" : "import-budget";
      return diagnostic;
    });

    this.#collection.set(uri, diagnostics);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#collection.dispose();
  }
}
