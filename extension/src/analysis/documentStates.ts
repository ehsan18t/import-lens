import type { ImportResult } from "../ipc/protocol.js";
import { mergeRefreshedResults, type RefreshMergeOptions } from "./refreshMerge.js";
import type { ImportAnalysisState } from "./state.js";

export interface RefreshApplyOptions extends RefreshMergeOptions {
  /**
   * The analysis generation this push was computed for (the daemon echoes the analysis's
   * request id). It is what lets a push that raced its own response be replayed onto THAT
   * response's states and no other — see {@link DocumentAnalysisStates.set}. Absent (older
   * daemon) → the push is replayed onto whatever states land next, which is the behaviour that
   * predates generations.
   */
  generation?: number;
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
 * Bound on the pushes held for one document between the response frame that opens an analysis and
 * the states that close it. In practice that window holds at most one document's imports, and the
 * queue is emptied by every {@link DocumentAnalysisStates.set}. The cap exists so a pathological
 * daemon (or a document whose analysis is superseded forever and never closed) cannot grow it
 * without limit; the OLDEST batch is dropped, because the newest results are the ones the user is
 * waiting on.
 */
const maxQueuedRefreshBatches = 256;

/**
 * The document → import-states map, plus the queue that makes a pushed import result survive the
 * response it raced.
 *
 * The race is not hypothetical and it is not rare: the daemon writes the `analyze_document_response`
 * and the first `refreshed_results` push to the same socket, and both frames routinely arrive in ONE
 * read. `IpcClient` dispatches every frame in a chunk synchronously, so the push reaches this class
 * while the `await` in `listener.analyze()` — the very continuation that stores the response's
 * states — has not run yet.
 *
 * There are two halves to that, and only fixing one of them fixed nothing:
 *
 * * On a COLD document there are no states for the push to merge into, and it used to early-return.
 * * On every RE-analysis — which is to say every time the user types — there ARE states: the ones
 *   the previous analysis left behind. The push merged into those happily, and then {@link set}
 *   installed the new response's states straight over the top of it. The push was not dropped; it
 *   was *overwritten*, which looks identical from the decoration's point of view: that import sat at
 *   "Calculating..." until the next edit. This is the steady state, so it was the common case.
 *
 * So a push is not merely applied, it is REMEMBERED — tagged with the analysis generation the daemon
 * computed it for — and {@link set} replays the pushes belonging to the generation it is installing
 * as part of the same update. A push cannot simply create the state it belongs to: it carries an
 * import's identity and its size, not the document range the decoration hangs on.
 *
 * Kept vscode-free (`AnalysisStore` wraps it and owns the change event) so the ordering can be
 * driven from a plain `node --test` harness against the real `IpcClient`.
 */
export class DocumentAnalysisStates {
  readonly #states = new Map<string, ImportAnalysisState[]>();
  readonly #queued = new Map<string, QueuedRefresh[]>();

  /**
   * Store the states of analysis `generation`, and re-apply every push that belongs to it — whether
   * it arrived before these states existed (a cold document) or merged into the states these are
   * replacing (a re-analysis). Returns the stored states so the caller can fire one change event for
   * the whole update.
   *
   * Pushes for any OTHER generation are dropped rather than replayed: a push the daemon computed for
   * an analysis a newer one has superseded must never fill in a state of the newer one (FR-004a).
   * They cannot be numerous — the listener drops a superseded push on arrival — but an analysis that
   * is abandoned mid-flight (its response overtaken) leaves its pushes behind, and this is where
   * they go.
   */
  set(key: string, states: ImportAnalysisState[], generation?: number): ImportAnalysisState[] {
    this.#states.set(key, states);

    const queued = this.#queued.get(key);
    this.#queued.delete(key);

    for (const push of queued ?? []) {
      if (belongsToGeneration(push, generation)) {
        this.#merge(key, push.results, push.options);
      }
    }

    return this.get(key);
  }

  /**
   * Replace a document's states with a recomputation OVER those same states (re-applying insights
   * after a config change). It opens no analysis and closes none, so it leaves the queue alone: the
   * pushes held there belong to an analysis that is still in flight and are still owed to its
   * {@link set}.
   */
  replace(key: string, states: ImportAnalysisState[]): void {
    this.#states.set(key, states);
  }

  get(key: string): ImportAnalysisState[] {
    return this.#states.get(key) ?? [];
  }

  /**
   * Merge pushed import results into a document's states, matched by per-import identity, and hold
   * the push for {@link set} to replay. Returns whether the stored states changed, so the caller
   * knows whether to re-render — a push against a document with no states yet changes nothing on
   * screen, and returns `false`.
   *
   * A superseded batch is neither applied nor held: it describes a document state the user has
   * already left.
   */
  applyRefreshedResults(
    key: string,
    results: readonly ImportResult[],
    options: RefreshApplyOptions = {},
  ): boolean {
    if (options.isCurrent === false) {
      return false;
    }

    this.#queue(key, results, options);

    if (!this.#states.has(key)) {
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

// An unstamped push (older daemon) has no generation to disagree with, and the merge is ungated for
// it everywhere else too; an unstamped `set` is likewise a caller with no generation to enforce.
const belongsToGeneration = (push: QueuedRefresh, generation: number | undefined): boolean =>
  push.options.generation === undefined ||
  generation === undefined ||
  push.options.generation === generation;
