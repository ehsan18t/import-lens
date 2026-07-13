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
 * how a push that raced its own response survives it — to the vscode-free core.
 */
export class AnalysisStore implements vscode.Disposable {
  readonly #documents = new DocumentAnalysisStates();
  readonly #onDidChange = new vscode.EventEmitter<vscode.Uri>();

  readonly onDidChange: vscode.Event<vscode.Uri> = this.#onDidChange.event;

  /**
   * Store the states an analysis produced. `generation` is that analysis's request id, and it is
   * not optional bookkeeping: it is what tells the core which held pushes belong to these states
   * and must be re-applied over them (see {@link DocumentAnalysisStates.set}).
   *
   * To recompute over the states already stored — insights after a config change — use
   * {@link replace}, which claims no generation and holds no pushes to account for.
   */
  set(uri: vscode.Uri, states: ImportAnalysisState[], generation: number): void {
    this.#documents.set(uri.toString(), states, generation);
    this.#onDidChange.fire(uri);
  }

  replace(uri: vscode.Uri, states: ImportAnalysisState[]): void {
    this.#documents.replace(uri.toString(), states);
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
   * A push that arrives in the same socket chunk as the response it belongs to is also HELD, so
   * that the `set` storing that response re-applies it rather than overwriting it. See
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
