import * as vscode from "vscode";
import { importAnalysisStateFromDaemon } from "./analysis/daemonState.js";
import { DebouncedDocumentScheduler } from "./analysis/debouncedDocumentScheduler.js";
import { documentFileCost } from "./analysis/fileSize.js";
import { AnalysisFreshnessTracker } from "./analysis/freshness.js";
import { changedLinesForFile } from "./analysis/gitDiff.js";
import {
  type BundleImpactHistoryStore,
  type ImportCostHistoryItem,
  importCostHistoryKey,
  recordImportCostHistory,
} from "./analysis/history.js";
import { applyImportAnalysisInsights } from "./analysis/insights.js";
import { ImportResultLogTracker } from "./analysis/resultLogging.js";
import type { AnalysisStore, ImportAnalysisState } from "./analysis/state.js";
import { getImportLensConfig } from "./config.js";
import type { DaemonManager } from "./daemon/manager.js";
import {
  type FileSizeDocumentResponse,
  type ImportResult,
  protocolVersion,
  type RefreshedImportIdentity,
} from "./ipc/protocol.js";
import { nextIpcRequestId } from "./ipc/requestIds.js";
import { supportedLanguageIds } from "./languages.js";
import type { ImportLensLogger } from "./logger.js";
import { bytesForCompression, formatBytes, labelForCompression } from "./ui/format.js";
import type { StatusBarController, StatusBarState } from "./ui/statusbar.js";
import { analysisRootForFile } from "./workspaceContext.js";

/** The inputs a File Cost re-read needs, and the generation they belong to. @see DocumentAnalysisController.refetchFileSizeWhenSettled */
interface FileSizeContext {
  document: vscode.TextDocument;
  workspaceRoot: string;
  generation: number;
  /** One re-read per analysis: the re-read itself can trigger another SWR push, and a push that re-reads on every push is a loop. */
  refetched: boolean;
}

export class DocumentAnalysisController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #daemon: DaemonManager;
  readonly #historyStore: BundleImpactHistoryStore;
  readonly #logger: ImportLensLogger;
  readonly #statusBar: StatusBarController;
  readonly #scheduler = new DebouncedDocumentScheduler();
  readonly #freshness = new AnalysisFreshnessTracker();
  // Working-tree changed lines per document, kept from the analysis that opened the
  // current generation so a pushed import can be captioned with its git delta without
  // shelling out to `git diff` once per import. Dropped when the document closes.
  readonly #changedLines = new Map<string, ReadonlySet<number>>();
  // What a streamed push needs in order to re-read the file's size once the document has finished
  // streaming: the document itself, the root it was analyzed against, and the generation that owns
  // both. A push carries only a URI (`daemon.onRefreshedResults`), and the File Cost is fetched per
  // document, per workspace root. Dropped when the document closes.
  readonly #analysisContexts = new Map<string, FileSizeContext>();

  constructor(
    context: vscode.ExtensionContext,
    store: AnalysisStore,
    daemon: DaemonManager,
    logger: ImportLensLogger,
    statusBar: StatusBarController,
  ) {
    this.#store = store;
    this.#daemon = daemon;
    this.#historyStore = context.globalState;
    this.#logger = logger;
    this.#statusBar = statusBar;

    context.subscriptions.push(
      vscode.workspace.onDidChangeTextDocument((event) => this.schedule(event.document)),
      vscode.workspace.onDidOpenTextDocument((document) => this.schedule(document)),
      vscode.workspace.onDidCloseTextDocument((document) => this.disposeDocument(document)),
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (
          editor &&
          supportedLanguageIds.has(editor.document.languageId) &&
          editor.document.uri.scheme === "file"
        ) {
          this.schedule(editor.document);
        } else {
          this.#statusBar.setState({ kind: "ready" });
        }
      }),
    );
  }

  schedule(document: vscode.TextDocument): void {
    if (!supportedLanguageIds.has(document.languageId) || document.uri.scheme !== "file") {
      return;
    }

    this.#scheduler.schedule(
      document.uri.toString(),
      getImportLensConfig().debounceMs,
      () => void this.analyze(document),
    );
  }

  /**
   * Apply a daemon push carrying import results the analysis response did not have:
   * an import whose engine build landed after the response went out, or a background
   * stale-while-revalidate refresh. Both merge the same way.
   *
   * Gated by the SAME freshness generation that guards `updateFileSize`: if a newer
   * analysis has superseded the generation this batch was computed for (the user
   * edited past it), the push is dropped rather than overwriting the current states.
   * Both `identities` and `generation` are optional so an older daemon still merges
   * (specifier-keyed, ungated).
   *
   * The insights are recomputed over the merged states, because a pushed size is a
   * number nobody has captioned yet: a cold document's imports ALL arrive this way, so
   * without this they would show a size with no "over budget", no git delta and no
   * shared-module note until the next edit. The git diff is not re-run — it is the one
   * expensive input, it belongs to the document rather than the import, and the analysis
   * that opened this generation already paid for it.
   */
  applyRefreshedResults(
    uri: vscode.Uri,
    results: ImportResult[],
    identities?: RefreshedImportIdentity[],
    generation?: number,
  ): void {
    const documentKey = uri.toString();
    const isCurrent =
      generation === undefined ? true : this.#freshness.isCurrent(documentKey, generation);
    const config = getImportLensConfig();
    const changedLines = this.#changedLines.get(documentKey);

    this.#store.applyRefreshedResults(uri, results, {
      identities,
      isCurrent,
      // The generation travels WITH the push into the store, because a push that arrived in the
      // same socket chunk as its own response is re-applied by the `set` that stores that
      // response's states — and only by that one.
      generation,
      refine: (states) =>
        applyImportAnalysisInsights(states, {
          changedLines,
          importCostHistory: this.#historyStore.get<ImportCostHistoryItem[]>(
            importCostHistoryKey,
            [],
          ),
          budgets: config.budgets,
        }),
    });

    // A streamed import is the first time its size is ever known, so this is where its
    // history row gets written — the trend insight on the next analysis reads it back.
    // `recordImportCostHistory` serializes its writes and skips unchanged rows, so a push
    // that merged nothing costs nothing.
    if (isCurrent) {
      // The STORE applies the gate — it takes the states, not rows built for it.
      void recordImportCostHistory(this.#historyStore, this.#store.get(uri)).catch(
        (error: unknown) => {
          this.#logger.warn(
            `Import history update failed: ${error instanceof Error ? error.message : String(error)}`,
          );
        },
      );
      this.refetchFileSizeWhenSettled(uri, generation);
    }
  }

  /**
   * Re-read the file's size once the last streamed import has landed.
   *
   * The size read the analysis made ran while the document was COLD: every import was still being
   * built, and any one of them that had not been measured yet made the file's total a floor
   * (`incomplete`) — a number that may not be judged against a budget at all (ADR-0006, invariant
   * 5). Without this, that "not evaluated" is the verdict the file keeps until the user's next
   * keystroke, on a document whose imports are now all measured and whose File Cost is now real.
   *
   * Once per analysis, and only when nothing is loading: the re-read is itself a `file_size_document`
   * request, which can serve stale and push a refresh of its own, and a re-read armed by every push
   * is a loop.
   */
  private refetchFileSizeWhenSettled(uri: vscode.Uri, generation?: number): void {
    const context = this.#analysisContexts.get(uri.toString());

    if (!context || context.refetched || generation !== context.generation) {
      return;
    }

    const states = this.#store.get(uri);

    if (states.length === 0 || states.some((state) => state.status === "loading")) {
      return;
    }

    context.refetched = true;
    void this.updateFileSize(context.document, context.workspaceRoot, context.generation);
  }

  async analyze(document: vscode.TextDocument): Promise<void> {
    const config = getImportLensConfig();
    const documentKey = document.uri.toString();
    const requestId = this.#freshness.begin(documentKey, nextIpcRequestId());

    if (!config.enabled || !supportedLanguageIds.has(document.languageId)) {
      this.#store.clear(document.uri);
      return;
    }

    const changedLinesPromise = changedLinesForFile(document.fileName, document.getText());
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (this.#daemon.state !== "ready" && (await this.#daemon.start(workspaceRoot)) !== "ready") {
      this.#store.clear(document.uri);
      this.setStatusForActive(document, { kind: "unavailable" });
      return;
    }

    this.setStatusForActive(document, { kind: "computing" });
    this.#logger.debug(`Starting document analysis request ${requestId}.`);
    // What a streamed push will need to re-read the File Cost when the document settles: a push
    // knows only the document's path.
    this.#analysisContexts.set(documentKey, {
      document,
      workspaceRoot,
      generation: requestId,
      refetched: false,
    });

    try {
      const resultLogger = new ImportResultLogTracker(
        this.#logger.child({ component: "analysis" }),
        requestId,
      );
      const response = await this.#daemon.analyzeDocument({
        type: "analyze_document",
        version: protocolVersion,
        request_id: requestId,
        workspace_root: workspaceRoot,
        active_document_path: document.fileName,
        source: document.getText(),
      });

      if (!response) {
        this.#store.clear(document.uri);
        this.setStatusForActive(document, { kind: "unavailable" });
        return;
      }

      if (!this.#freshness.isCurrent(documentKey, response.request_id)) {
        return;
      }

      if (response.error) {
        this.#logger.warn(`Document analysis failed: ${response.error}`);
        this.#store.clear(document.uri);
        this.setStatusForActive(document, { kind: "unavailable" });
        return;
      }

      if (response.imports.length === 0) {
        this.#store.clear(document.uri);
        this.setStatusForActive(document, { kind: "ready" });
        return;
      }

      const responseStates = response.imports.map((item) =>
        importAnalysisStateFromDaemon(item, (specifier, reason) =>
          resultLogger.logMissingResult(specifier, reason),
        ),
      );

      for (const state of responseStates) {
        if (state.status === "ready" && state.result) {
          resultLogger.logResult(state.result);
        }
      }

      // Store the response BEFORE awaiting anything else, and stamp it with the generation
      // it belongs to. The daemon answers a cold import `loading` and pushes its size the
      // moment its build lands — which can be in the same socket chunk as this very response,
      // long before the git diff below resolves. The generation is what lets the store re-apply
      // such a push over these states instead of letting them overwrite it (see
      // `DocumentAnalysisStates`); without it, on every re-analysis that import went back to
      // "Calculating..." and stayed there until the next edit. Storing early also paints the
      // cache hits at once, which is the point of the whole exercise.
      //
      // No refiner: this analysis has not run its `git diff` yet, so it has nothing to caption
      // ANY of these states with — the cache hits in `responseStates` are uncaptioned too. The
      // captions arrive together, below, when the inputs they are derived from do.
      this.#store.set(document.uri, responseStates, requestId);

      const history = this.#historyStore.get<ImportCostHistoryItem[]>(importCostHistoryKey, []);
      const changedLines = await changedLinesPromise;

      if (!this.#freshness.isCurrent(documentKey, requestId)) {
        return;
      }
      this.#changedLines.set(documentKey, changedLines ?? new Set());

      // Read the states back rather than reusing `responseStates`: a pushed import may
      // already have landed in them during the await above, and overwriting the store
      // with the pre-push snapshot would undo it.
      const currentStates = this.#store.get(document.uri);
      // ONE refiner, for the states this analysis stores AND for the pushes the store replays
      // over them: a pushed import is captioned from the same git diff, history and budgets as
      // every other import in the document, and never from what a push happened to capture
      // mid-analysis (which predates the diff, and would strip the working-tree badge off the
      // whole document).
      const refine = (states: ImportAnalysisState[]): ImportAnalysisState[] =>
        applyImportAnalysisInsights(states, {
          changedLines,
          importCostHistory: history,
          budgets: config.budgets,
        });
      this.#store.set(document.uri, refine(currentStates), requestId, refine);
      try {
        await recordImportCostHistory(this.#historyStore, currentStates);
      } catch (error) {
        this.#logger.warn(
          `Import history update failed: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
      await this.updateFileSize(document, workspaceRoot, requestId);
      this.#logger.debug(`Completed document analysis request ${requestId}.`);
    } catch (error) {
      this.#logger.warn(
        `Analysis request failed: ${error instanceof Error ? error.message : String(error)}`,
      );
      this.#store.clear(document.uri);
      this.setStatusForActive(document, { kind: "unavailable" });
    }
  }

  private async updateFileSize(
    document: vscode.TextDocument,
    workspaceRoot: string,
    analysisRequestId: number,
  ): Promise<void> {
    const documentKey = document.uri.toString();
    const config = getImportLensConfig();
    let response: FileSizeDocumentResponse | null = null;
    try {
      response = await this.#daemon.requestFileSizeDocument({
        type: "file_size_document",
        version: protocolVersion,
        request_id: nextIpcRequestId(),
        workspace_root: workspaceRoot,
        active_document_path: document.fileName,
        source: document.getText(),
        // Tag this size read with the analysis generation it belongs to. The daemon
        // echoes it on the resulting SWR refresh push so we can drop a push that a
        // newer analysis has since superseded (see applyRefreshedResults).
        analysis_generation: analysisRequestId,
      });
    } catch (error) {
      this.#logger.warn(
        `File-size status request failed: ${error instanceof Error ? error.message : String(error)}`,
      );
    }

    // A newer analysis for this document supersedes this size result.
    if (!this.#freshness.isCurrent(documentKey, analysisRequestId)) {
      return;
    }
    // The File Cost goes into the STORE, because the surface that judges it — the per-file budget
    // diagnostic — reads the store and never sees this response. That is the whole defect: the
    // budget checker could not reach this number, so it re-derived one by SUMMING the per-import
    // costs, which counts a shared graph once per import and warned files that were inside budget
    // (ADR-0004). It is stored with the daemon's honesty flags and judged through the one predicate
    // (`isDurableFileSize`); a floor or an over-count leaves the budget *not evaluated*, never
    // "under".
    //
    // `undefined` withdraws it: a failed round-trip is not evidence that the previous number still
    // describes this document.
    this.#store.setFileCost(
      document.uri,
      response && !response.error ? documentFileCost(response) : undefined,
    );
    if (!response || response.error) {
      // Nothing was summed at all, so there is no number to show — the ONE thing the aggregate's
      // `error` legitimately answers. It is not the question "is this number usable?": `incomplete`
      // and `degraded` answer that, and they are consulted below.
      //
      // Analysis itself succeeded (decorations are shown); a failed size round-trip should not read
      // as "Unavailable".
      this.setStatusForActive(document, { kind: "ready" });
      return;
    }
    // `states`, not `imports`: the daemon answers a cold import `loading` and `imports`
    // carries only the ones it has measured, so on a cold document that list is empty
    // while the file's own total — which comes from the combined build, not from the
    // per-import measurements — is perfectly real. Gating the label on `imports` would
    // hide the size of exactly the documents the user just opened.
    if (response.states.length === 0) {
      this.setStatusForActive(document, { kind: "ready" });
      return;
    }
    // A floor (`incomplete`) or an un-deduplicated per-import sum (`degraded`) is worth SHOWING —
    // FR-024a, a floor beats a blank — but it is not the file's size, and the status bar is the one
    // surface that shows the number with no diagnostics beside it to say so. `~` is the codebase's
    // existing mark for "this figure is approximate" (FR-031).
    const approximate = response.incomplete === true || response.degraded === true;
    const label = `${approximate ? "~" : ""}${formatBytes(bytesForCompression(response, config.compression))} ${labelForCompression(config.compression)}`;
    this.setStatusForActive(document, { kind: "size", label });
  }

  private setStatusForActive(document: vscode.TextDocument, state: StatusBarState): void {
    // The status bar reflects the active editor, so a late-completing analysis
    // for a now-inactive document must not overwrite the active file's status.
    if (vscode.window.activeTextEditor?.document.uri.toString() === document.uri.toString()) {
      this.#statusBar.setState(state);
    }
  }

  private disposeDocument(document: vscode.TextDocument): void {
    const key = document.uri.toString();
    this.#scheduler.cancel(key);
    this.#freshness.forget(key);
    this.#changedLines.delete(key);
    this.#analysisContexts.delete(key);
    this.#store.clear(document.uri);
  }

  dispose(): void {
    this.#scheduler.dispose();
    this.#freshness.clear();
    this.#changedLines.clear();
    this.#analysisContexts.clear();
  }
}
