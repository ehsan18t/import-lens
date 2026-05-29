import * as vscode from "vscode";
import type { DetectedImport } from "../imports/types.js";
import type { ImportResult } from "../ipc/protocol.js";

export type ImportAnalysisStatus = "loading" | "ready" | "missing" | "unavailable";

export interface ImportAnalysisState {
  detected: DetectedImport;
  status: ImportAnalysisStatus;
  result?: ImportResult;
  message?: string;
}

export class AnalysisStore implements vscode.Disposable {
  readonly #states = new Map<string, ImportAnalysisState[]>();
  readonly #onDidChange = new vscode.EventEmitter<vscode.Uri>();

  readonly onDidChange: vscode.Event<vscode.Uri> = this.#onDidChange.event;

  set(uri: vscode.Uri, states: ImportAnalysisState[]): void {
    this.#states.set(uri.toString(), states);
    this.#onDidChange.fire(uri);
  }

  get(uri: vscode.Uri): ImportAnalysisState[] {
    return this.#states.get(uri.toString()) ?? [];
  }

  clear(uri: vscode.Uri): void {
    this.#states.delete(uri.toString());
    this.#onDidChange.fire(uri);
  }

  all(): ImportAnalysisState[] {
    return [...this.#states.values()].flat();
  }

  dispose(): void {
    this.#onDidChange.dispose();
  }
}
