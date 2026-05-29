import * as vscode from "vscode";

export type ImportLensStatus = "ready" | "computing" | "unavailable";

const labels: Record<ImportLensStatus, string> = {
  ready: "ImportLens: Ready",
  computing: "ImportLens: Computing...",
  unavailable: "ImportLens: Unavailable",
};

export class StatusBarController implements vscode.Disposable {
  readonly #item: vscode.StatusBarItem;

  constructor() {
    this.#item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.#item.name = "ImportLens";
    this.#item.command = "importLens.showLogs";
    this.#item.text = labels.unavailable;
    this.#item.show();
  }

  setStatus(status: ImportLensStatus): void {
    this.#item.text = labels[status];
  }

  dispose(): void {
    this.#item.dispose();
  }
}
