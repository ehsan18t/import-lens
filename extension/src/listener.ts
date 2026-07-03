import * as vscode from "vscode";
import { AnalysisFreshnessTracker } from "./analysis/freshness.js";
import { changedLinesForFile } from "./analysis/gitDiff.js";
import {
  applyImportAnalysisInsights,
  importCostHistoryItemsForStates,
} from "./analysis/insights.js";
import {
  importCostHistoryKey,
  recordImportCostHistory,
  type BundleImpactHistoryStore,
  type ImportCostHistoryItem,
} from "./analysis/history.js";
import { ImportResultLogTracker } from "./analysis/resultLogging.js";
import type { AnalysisStore, ImportAnalysisState } from "./analysis/state.js";
import { getImportLensConfig } from "./config.js";
import type { DaemonManager } from "./daemon/manager.js";
import { supportedLanguageIds } from "./languages.js";
import type { ImportLensLogger } from "./logger.js";
import type { StatusBarController } from "./ui/statusbar.js";
import { protocolVersion, type ImportAnalysisItem } from "./ipc/protocol.js";
import { nextIpcRequestId } from "./ipc/requestIds.js";
import { analysisRootForFile } from "./workspaceContext.js";

export class DocumentAnalysisController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #daemon: DaemonManager;
  readonly #historyStore: BundleImpactHistoryStore;
  readonly #logger: ImportLensLogger;
  readonly #statusBar: StatusBarController;
  readonly #timers = new Map<string, NodeJS.Timeout>();
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
        if (editor) {
          this.schedule(editor.document);
        }
      }),
    );
  }

  schedule(document: vscode.TextDocument): void {
    if (!supportedLanguageIds.has(document.languageId) || document.uri.scheme !== "file") {
      return;
    }

    const config = getImportLensConfig();
    const key = document.uri.toString();
    const existing = this.#timers.get(key);

    if (existing) {
      clearTimeout(existing);
    }

    this.#timers.set(key, setTimeout(() => void this.analyze(document), config.debounceMs));
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

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      this.#store.clear(document.uri);
      this.#statusBar.setStatus("unavailable");
      return;
    }

    this.#statusBar.setStatus("computing");
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
        this.#statusBar.setStatus("unavailable");
        return;
      }

      if (!this.#freshness.isCurrent(documentKey, response.request_id)) {
        return;
      }

      if (response.error) {
        this.#logger.warn(`Document analysis failed: ${response.error}`);
        this.#store.clear(document.uri);
        this.#statusBar.setStatus("unavailable");
        return;
      }

      if (response.imports.length === 0) {
        this.#store.clear(document.uri);
        this.#statusBar.setStatus("ready");
        return;
      }

      const responseStates = response.imports.map((item) =>
        importAnalysisStateFromDaemon(item, (specifier, reason) => resultLogger.logMissingResult(specifier, reason)));

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
        await recordImportCostHistory(this.#historyStore, importCostHistoryItemsForStates(responseStates));
      } catch (error) {
        this.#logger.warn(`Import history update failed: ${error instanceof Error ? error.message : String(error)}`);
      }
      this.#statusBar.setStatus("ready");
      this.#logger.debug(`Completed document analysis request ${requestId}.`);
    } catch (error) {
      this.#logger.warn(`Analysis request failed: ${error instanceof Error ? error.message : String(error)}`);
      this.#store.clear(document.uri);
      this.#statusBar.setStatus("unavailable");
    }
  }

  private disposeDocument(document: vscode.TextDocument): void {
    const key = document.uri.toString();
    const timer = this.#timers.get(key);

    if (timer) {
      clearTimeout(timer);
      this.#timers.delete(key);
    }

    this.#freshness.forget(key);
    this.#store.clear(document.uri);
  }

  dispose(): void {
    for (const timer of this.#timers.values()) {
      clearTimeout(timer);
    }

    this.#timers.clear();
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
