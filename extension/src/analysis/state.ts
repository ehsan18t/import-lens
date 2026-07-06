import * as vscode from "vscode";
import type { DetectedImport, ImportResult } from "../ipc/protocol.js";
import { mergeRefreshedResults, type RefreshMergeOptions } from "./refreshMerge.js";

export type ImportAnalysisStatus = "loading" | "ready" | "missing" | "unavailable";

export interface ImportAnalysisInsight {
  label?: string;
  tooltip: string;
}

export interface ImportAnalysisState {
  detected: DetectedImport;
  status: ImportAnalysisStatus;
  result?: ImportResult;
  message?: string;
  insights?: ImportAnalysisInsight[];
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

  /**
   * Merge background-refreshed sizes (from the daemon's stale-while-revalidate
   * push) into an existing document's states, matched by per-import identity, and
   * fire onDidChange so decorations re-render in place. No-op if the document has
   * no states (e.g. it was closed), nothing matched, or the batch was superseded
   * (`options.isCurrent === false`). The caller supplies the identity/supersession
   * options; see `mergeRefreshedResults`.
   */
  applyRefreshedResults(
    uri: vscode.Uri,
    results: ImportResult[],
    options?: RefreshMergeOptions,
  ): void {
    const existing = this.#states.get(uri.toString());

    if (!existing) {
      return;
    }

    const { next, changed } = mergeRefreshedResults(existing, results, options);

    if (changed) {
      this.#states.set(uri.toString(), next);
      this.#onDidChange.fire(uri);
    }
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
