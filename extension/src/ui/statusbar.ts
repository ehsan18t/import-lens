import * as vscode from "vscode";
import { type StatusBarState, statusBarText, statusBarTooltip } from "./statusbarText.js";

export type { StatusBarState } from "./statusbarText.js";

export class StatusBarController implements vscode.Disposable {
  readonly #item: vscode.StatusBarItem;

  constructor() {
    this.#item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.#item.name = "ImportLens";
    this.#item.command = "importLens.showLogs";
    this.setState({ kind: "unavailable" });
    this.#item.show();
  }

  setState(state: StatusBarState): void {
    this.#item.text = statusBarText(state);
    this.#item.tooltip = statusBarTooltip(state);
  }

  dispose(): void {
    this.#item.dispose();
  }
}
