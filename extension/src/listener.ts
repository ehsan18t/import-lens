import * as vscode from "vscode";
import { DebouncedDocumentScheduler } from "./analysis/debouncedDocumentScheduler.js";
import { AnalysisFreshnessTracker } from "./analysis/freshness.js";
import { changedLinesForFile } from "./analysis/gitDiff.js";
import {
  type BundleImpactHistoryStore,
  type ImportCostHistoryItem,
  importCostHistoryKey,
  recordImportCostHistory,
} from "./analysis/history.js";
import {
  applyImportAnalysisInsights,
  importCostHistoryItemsForStates,
} from "./analysis/insights.js";
import { ImportResultLogTracker } from "./analysis/resultLogging.js";
import type { AnalysisStore, ImportAnalysisState } from "./analysis/state.js";
import { getImportLensConfig } from "./config.js";
import type { DaemonManager } from "./daemon/manager.js";
import {
  type FileSizeDocumentResponse,
  type ImportAnalysisItem,
  protocolVersion,
} from "./ipc/protocol.js";
import { nextIpcRequestId } from "./ipc/requestIds.js";
import { supportedLanguageIds } from "./languages.js";
import type { ImportLensLogger } from "./logger.js";
import { bytesForCompression, formatBytes, labelForCompression } from "./ui/format.js";
import type { StatusBarController, StatusBarState } from "./ui/statusbar.js";
import { analysisRootForFile } from "./workspaceContext.js";

export class DocumentAnalysisController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #daemon: DaemonManager;
  readonly #historyStore: BundleImpactHistoryStore;
  readonly #logger: ImportLensLogger;
  readonly #statusBar: StatusBarController;
  readonly #scheduler = new DebouncedDocumentScheduler();
  readonly #freshness = new AnalysisFreshnessTracker();

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

      const history = this.#historyStore.get<ImportCostHistoryItem[]>(importCostHistoryKey, []);
      const nextStates = applyImportAnalysisInsights(responseStates, {
        changedLines: await changedLinesPromise,
        importCostHistory: history,
        budgets: config.budgets,
      });

      this.#store.set(document.uri, nextStates);
      try {
        await recordImportCostHistory(
          this.#historyStore,
          importCostHistoryItemsForStates(responseStates),
        );
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
    if (!response || response.error) {
      // Analysis itself succeeded (decorations are shown); a failed size
      // round-trip should not read as "Unavailable".
      this.setStatusForActive(document, { kind: "ready" });
      return;
    }
    if (response.imports.length === 0) {
      this.setStatusForActive(document, { kind: "ready" });
      return;
    }
    const label = `${formatBytes(bytesForCompression(response, config.compression))} ${labelForCompression(config.compression)}`;
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
    this.#store.clear(document.uri);
  }

  dispose(): void {
    this.#scheduler.dispose();
    this.#freshness.clear();
  }
}

const importAnalysisStateFromDaemon = (
  item: ImportAnalysisItem,
  logMissingResult: (specifier: string, reason: string) => void,
): ImportAnalysisState => {
  if (item.status === "ready" && item.result) {
    return {
      detected: item.detected,
      status: "ready",
      result: item.result,
    };
  }

  if (item.status === "missing") {
    logMissingResult(item.detected.specifier, item.message ?? "Package not found");
    return {
      detected: item.detected,
      status: "missing",
      message: item.message ?? "Package not found",
    };
  }

  return {
    detected: item.detected,
    status: "unavailable",
    message: item.message ?? "Daemon unavailable",
  };
};
