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
   * Merge pushed import results (a background stale-while-revalidate refresh, or an
   * import whose engine build landed after its analysis response) into an existing
   * document's states, matched by per-import identity, and fire onDidChange so
   * decorations re-render in place. No-op if the document has no states (e.g. it was
   * closed), nothing matched, or the batch was superseded (`options.isCurrent ===
   * false`). The caller supplies the identity/supersession options; see
   * `mergeRefreshedResults`.
   *
   * `refine` runs on the merged states before they are stored, in the SAME update: a
   * pushed size is a new number, and the insights that caption it (over budget, git
   * delta, shared modules) are derived from it. Recomputing them in a second `set`
   * would fire a second render and briefly show the number without its caption.
   */
  applyRefreshedResults(
    uri: vscode.Uri,
    results: ImportResult[],
    options?: RefreshMergeOptions & {
      refine?: (states: ImportAnalysisState[]) => ImportAnalysisState[];
    },
  ): void {
    const existing = this.#states.get(uri.toString());

    if (!existing) {
      return;
    }

    const { next, changed } = mergeRefreshedResults(existing, results, options);

    if (changed) {
      this.#states.set(uri.toString(), options?.refine ? options.refine(next) : next);
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
