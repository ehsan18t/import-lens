import type { ImportResult } from "../ipc/protocol.js";
import type { DocumentFileCost } from "./fileSize.js";
import { mergeRefreshedResults, type RefreshMergeOptions } from "./refreshMerge.js";
import type { ImportAnalysisState } from "./state.js";

/**
 * Recomputes the insights that caption a size — over budget, the git working-tree delta, the
 * shared-module note — over the states they belong to, in the SAME update that installs the size.
 * Recomputing them in a second store write would fire a second render and briefly show the number
 * without its caption.
 *
 * A refiner is only as good as the inputs it closed over, and those inputs belong to an ANALYSIS
 * (the git diff it ran for this document, the history it read, the budgets in force) — never to a
 * push, which merely captures whatever the controller held when it landed.
 */
export type RefineStates = (states: ImportAnalysisState[]) => ImportAnalysisState[];

export interface RefreshApplyOptions extends RefreshMergeOptions {
  /**
   * The analysis generation this push was computed for (the daemon echoes the analysis's
   * request id). It is what lets a push that raced its own response be replayed onto THAT
   * response's states and no other — see {@link DocumentAnalysisStates.set}. Absent when the
   * daemon had no generation to echo (the SWR refresh of a size read the extension did not tag,
   * e.g. the "Show current file size" command's): such a push merges into the states on screen
   * when it arrives, and is never held for replay, because there is no analysis it can be shown
   * to belong to.
   */
  generation?: number;
  /** @see RefineStates — used for the live merge; a REPLAY is refined by {@link DocumentAnalysisStates.set}. */
  refine?: RefineStates;
}

/** A push held for the {@link DocumentAnalysisStates.set} that closes the analysis it belongs to. */
interface QueuedRefresh {
  results: readonly ImportResult[];
  /** Not optional here: a push with no generation is never queued, so a queued one always has one. */
  generation: number;
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
  readonly #fileCosts = new Map<string, DocumentFileCost>();

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
   *
   * `refine` is the ANALYSIS's refiner, and a replayed push is captioned with it — never with the
   * push's own. `states` arrive here already captioned from the analysis's real inputs; the push's
   * closure captured the inputs of the moment it landed, which is mid-analysis, BEFORE the `git
   * diff` this generation is captioned by has resolved. Running it over the merged states would
   * recompute the insights of the WHOLE document from those stale inputs and take the working-tree
   * badge off every import in it.
   */
  set(
    key: string,
    states: ImportAnalysisState[],
    generation: number,
    refine?: RefineStates,
  ): ImportAnalysisState[] {
    this.#states.set(key, states);
    // A new analysis is a new document. The File Cost in hand measured the file as it was BEFORE
    // the keystroke that opened this generation — the import the user just deleted is still in it —
    // and a budget judged against it is judged against a file that no longer exists. It is dropped
    // here and re-armed by the size read this analysis is about to make (`listener.updateFileSize`),
    // so between the two the file budget is simply **not evaluated**, which is the honest answer
    // while the number is unknown (ADR-0006, invariant 5).
    this.#fileCosts.delete(key);

    const queued = this.#queued.get(key);
    this.#queued.delete(key);

    for (const push of queued ?? []) {
      if (push.generation === generation) {
        this.#merge(key, push.results, push.options, refine);
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
   * The document's File Cost — the daemon's combined build over its imports, the one number the
   * per-file budget may be judged against ({@link DocumentFileCost}). `undefined` means the store
   * has not been told one for the document as it stands now, and the file budget is then not
   * evaluated: a summed alternative is a *different quantity* (ADR-0004), not a fallback.
   */
  setFileCost(key: string, fileCost: DocumentFileCost | undefined): void {
    if (fileCost === undefined) {
      this.#fileCosts.delete(key);
      return;
    }

    this.#fileCosts.set(key, fileCost);
  }

  fileCost(key: string): DocumentFileCost | undefined {
    return this.#fileCosts.get(key);
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
    this.#fileCosts.delete(key);
  }

  all(): ImportAnalysisState[] {
    return [...this.#states.values()].flat();
  }

  /**
   * `refine` overrides the push's own: {@link set} passes the analysis's refiner when it replays a
   * push, and the live merge (no override) uses the one the push arrived with.
   */
  #merge(
    key: string,
    results: readonly ImportResult[],
    options: RefreshApplyOptions,
    refine: RefineStates | undefined = options.refine,
  ): boolean {
    const existing = this.#states.get(key);

    if (!existing) {
      return false;
    }

    const { next, changed } = mergeRefreshedResults(existing, [...results], options);

    if (!changed) {
      return false;
    }

    this.#states.set(key, refine ? refine(next) : next);
    return true;
  }

  /**
   * Hold a push for the {@link set} that closes its analysis. A push with NO generation is not held:
   * it belongs to no analysis, so no later `set` can be shown to be the one it was computed for, and
   * replaying it onto whichever states land next is the resurrection {@link clear} exists to
   * prevent. It has already merged into the states on screen if there were any, which is all an
   * ungated push was ever owed.
   */
  #queue(key: string, results: readonly ImportResult[], options: RefreshApplyOptions): void {
    const { generation } = options;

    if (generation === undefined) {
      return;
    }

    const queued = this.#queued.get(key) ?? [];
    queued.push({ results, generation, options });

    while (queued.length > maxQueuedRefreshBatches) {
      queued.shift();
    }

    this.#queued.set(key, queued);
  }
}
