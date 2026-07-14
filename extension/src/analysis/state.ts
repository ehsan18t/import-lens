import * as vscode from "vscode";
import type { DetectedImport, ImportResult } from "../ipc/protocol.js";
import {
  DocumentAnalysisStates,
  type RefineStates,
  type RefreshApplyOptions,
} from "./documentStates.js";
import type { DocumentFileCost } from "./fileSize.js";

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
   * `refine` is how THIS analysis captions a size, and the core uses it for the pushes it replays,
   * so a pushed import is captioned from the same inputs as every other import in `states`.
   *
   * To recompute over the states already stored — insights after a config change — use
   * {@link replace}, which claims no generation and holds no pushes to account for.
   */
  set(
    uri: vscode.Uri,
    states: ImportAnalysisState[],
    generation: number,
    refine?: RefineStates,
  ): void {
    this.#documents.set(uri.toString(), states, generation, refine);
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
   * Hand the store the document's File Cost — the daemon's combined build over its imports — and
   * re-render, because the per-file budget diagnostic is judged against it and nothing else
   * ({@link DocumentFileCost}). `undefined` withdraws it: the size read failed, or its answer was
   * not this file's number, and the file budget goes back to "not evaluated".
   *
   * It lives here, beside the states, because the two are read together and by the same consumer:
   * `BudgetDiagnosticsController` refreshes off `onDidChange` and must see both. The controller that
   * FETCHES the File Cost (`listener.updateFileSize`) is not the one that judges it, which is
   * exactly why there was nowhere to put it and why the budget check summed the imports instead.
   */
  setFileCost(uri: vscode.Uri, fileCost: DocumentFileCost | undefined): void {
    this.#documents.setFileCost(uri.toString(), fileCost);
    this.#onDidChange.fire(uri);
  }

  fileCost(uri: vscode.Uri): DocumentFileCost | undefined {
    return this.#documents.fileCost(uri.toString());
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
