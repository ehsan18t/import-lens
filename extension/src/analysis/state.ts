import * as vscode from "vscode";
import type { DetectedImport, ImportResult } from "../ipc/protocol.js";
import { DocumentAnalysisStates, type RefreshApplyOptions } from "./documentStates.js";

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

/**
 * The vscode-facing wrapper around {@link DocumentAnalysisStates}: it owns the change event and
 * the `Uri` keying, and delegates every decision about what a document's states ARE — including
 * how a push that outran its response is held until the states exist — to the vscode-free core.
 */
export class AnalysisStore implements vscode.Disposable {
  readonly #documents = new DocumentAnalysisStates();
  readonly #onDidChange = new vscode.EventEmitter<vscode.Uri>();

  readonly onDidChange: vscode.Event<vscode.Uri> = this.#onDidChange.event;

  set(uri: vscode.Uri, states: ImportAnalysisState[]): void {
    this.#documents.set(uri.toString(), states);
    this.#onDidChange.fire(uri);
  }

  get(uri: vscode.Uri): ImportAnalysisState[] {
    return this.#documents.get(uri.toString());
  }

  /**
   * Merge pushed import results (a background stale-while-revalidate refresh, or an
   * import whose engine build landed after its analysis response) into an existing
   * document's states, matched by per-import identity, and fire onDidChange so
   * decorations re-render in place. No-op if nothing matched or the batch was superseded
   * (`options.isCurrent === false`).
   *
   * A push for a document whose states have not been stored yet is HELD, not dropped: the
   * response frame and the first push routinely arrive in one socket read, and the push is
   * dispatched before the awaited continuation that stores the response. See
   * {@link DocumentAnalysisStates}.
   */
  applyRefreshedResults(
    uri: vscode.Uri,
    results: ImportResult[],
    options?: RefreshApplyOptions,
  ): void {
    if (this.#documents.applyRefreshedResults(uri.toString(), results, options)) {
      this.#onDidChange.fire(uri);
    }
  }

  clear(uri: vscode.Uri): void {
    this.#documents.clear(uri.toString());
    this.#onDidChange.fire(uri);
  }

  all(): ImportAnalysisState[] {
    return this.#documents.all();
  }

  dispose(): void {
    this.#onDidChange.dispose();
  }
}
