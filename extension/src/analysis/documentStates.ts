import type { ImportResult } from "../ipc/protocol.js";
import { mergeRefreshedResults, type RefreshMergeOptions } from "./refreshMerge.js";
import type { ImportAnalysisState } from "./state.js";

export interface RefreshApplyOptions extends RefreshMergeOptions {
  /**
   * Runs on the merged states before they are stored, in the SAME update: a pushed size
   * is a new number, and the insights that caption it (over budget, git delta, shared
   * modules) are derived from it. Recomputing them in a second store write would fire a
   * second render and briefly show the number without its caption.
   */
  refine?: (states: ImportAnalysisState[]) => ImportAnalysisState[];
}

interface QueuedRefresh {
  results: readonly ImportResult[];
  options: RefreshApplyOptions;
}

/**
 * Bound on the pushes held for a document that has no states yet. The queue is only ever
 * non-empty inside the window described on {@link DocumentAnalysisStates.applyRefreshedResults}
 * — between a response frame being dispatched and its awaiting continuation storing the states
 * it carried — so in practice it holds at most one document's imports. The cap exists so a
 * pathological daemon (or a document whose analysis is superseded forever and never closed)
 * cannot grow it without limit; the OLDEST batch is dropped, because the newest results are the
 * ones the user is waiting on.
 */
const maxQueuedRefreshBatches = 256;

/**
 * The document → import-states map, plus the small queue that makes a pushed import result
 * deliverable when it arrives BEFORE the response that creates the state it belongs to.
 *
 * That ordering is not hypothetical, and it is not rare: the daemon writes the
 * `analyze_document_response` and the first `refreshed_results` push to the same socket, and
 * both frames routinely arrive in ONE read. `IpcClient` dispatches every frame in a chunk
 * synchronously, so the push reaches this class while the `await` in `listener.analyze()` — the
 * very continuation that stores the response's states — has not run yet. A push against an empty
 * document used to early-return, and that import sat at "Calculating..." for ever.
 *
 * A push cannot simply CREATE a state: it carries an import's identity and its size, not the
 * document range the decoration hangs on. So it waits for the states, and {@link set} drains the
 * queue in arrival order as part of the same update.
 *
 * Kept vscode-free (`AnalysisStore` wraps it and owns the change event) so the ordering can be
 * driven from a plain `node --test` harness against the real `IpcClient`.
 */
export class DocumentAnalysisStates {
  readonly #states = new Map<string, ImportAnalysisState[]>();
  readonly #queued = new Map<string, QueuedRefresh[]>();

  /**
   * Store a document's states and apply every push that arrived before them. Returns the stored
   * states so the caller can fire one change event for the whole update.
   */
  set(key: string, states: ImportAnalysisState[]): ImportAnalysisState[] {
    this.#states.set(key, states);

    const queued = this.#queued.get(key);
    this.#queued.delete(key);

    for (const push of queued ?? []) {
      this.#merge(key, push.results, push.options);
    }

    return this.get(key);
  }

  get(key: string): ImportAnalysisState[] {
    return this.#states.get(key) ?? [];
  }

  /**
   * Merge pushed import results into a document's states, matched by per-import identity.
   * Returns whether the stored states changed, so the caller knows whether to re-render.
   *
   * When the document has no states yet, the push is QUEUED rather than dropped, and {@link set}
   * applies it the moment the response's states land. Nothing changed on screen yet, so this
   * returns `false`.
   */
  applyRefreshedResults(
    key: string,
    results: readonly ImportResult[],
    options: RefreshApplyOptions = {},
  ): boolean {
    if (!this.#states.has(key)) {
      this.#queue(key, results, options);
      return false;
    }

    return this.#merge(key, results, options);
  }

  clear(key: string): void {
    this.#states.delete(key);
    // A document with no analysis has nothing for a push to merge into, and the pushes still
    // queued belong to the analysis that was just abandoned (closed document, failed or empty
    // response). Holding them would only let them merge into a LATER analysis of the same file.
    this.#queued.delete(key);
  }

  all(): ImportAnalysisState[] {
    return [...this.#states.values()].flat();
  }

  #merge(key: string, results: readonly ImportResult[], options: RefreshApplyOptions): boolean {
    const existing = this.#states.get(key);

    if (!existing) {
      return false;
    }

    const { next, changed } = mergeRefreshedResults(existing, [...results], options);

    if (!changed) {
      return false;
    }

    this.#states.set(key, options.refine ? options.refine(next) : next);
    return true;
  }

  #queue(key: string, results: readonly ImportResult[], options: RefreshApplyOptions): void {
    const queued = this.#queued.get(key) ?? [];
    queued.push({ results, options });

    while (queued.length > maxQueuedRefreshBatches) {
      queued.shift();
    }

    this.#queued.set(key, queued);
  }
}
