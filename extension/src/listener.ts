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
import { createImportRequest } from "./analysis/request.js";
import { ImportResultLogTracker } from "./analysis/resultLogging.js";
import { applyFinalBatchResults, markLoadingStatesUnavailable } from "./analysis/status.js";
import type { AnalysisStore, ImportAnalysisState } from "./analysis/state.js";
import { getImportLensConfig } from "./config.js";
import type { DaemonManager } from "./daemon/manager.js";
import { extractRuntimeImports } from "./imports/parser.js";
import { loadImportLensIgnore, shouldIgnoreImport } from "./imports/ignore.js";
import { getPackageName } from "./imports/specifier.js";
import { resolveInstalledPackagesByName } from "./imports/resolver.js";
import { supportedLanguageIds } from "./languages.js";
import type { ImportLensLogger } from "./logger.js";
import type { StatusBarController } from "./ui/statusbar.js";
import { applyStreamingBatchPartial } from "./analysis/batchPartial.js";
import { protocolVersion, type BatchResponse } from "./ipc/protocol.js";
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

    const ignoreRules = await loadImportLensIgnore(document.fileName);
    const imports = extractRuntimeImports(document.fileName, document.getText())
      .filter((detected) => !shouldIgnoreImport(detected, document.fileName, ignoreRules));

    if (imports.length === 0) {
      this.#store.clear(document.uri);
      return;
    }

    const states: ImportAnalysisState[] = [];
    const requestImports = [];
    const requestStateIndexes: number[] = [];
    const changedLinesPromise = changedLinesForFile(document.fileName);
    const packageResolutions = await resolveInstalledPackagesByName(
      imports.map((detected) => detected.specifier),
      document.fileName,
    );

    for (const detected of imports) {
      const resolution = packageResolutions.get(getPackageName(detected.specifier))
        ?? { ok: false as const, packageName: getPackageName(detected.specifier), reason: "package_not_found" as const };

      if (!resolution.ok) {
        states.push({
          detected,
          status: "missing",
          message: resolution.reason === "package_not_found" ? "Package not found" : "Invalid package.json",
        });
        continue;
      }

      states.push({ detected, status: "loading" });
      requestStateIndexes.push(states.length - 1);
      requestImports.push(createImportRequest(detected, resolution.version));
    }

    let currentStates = states;
    this.#store.set(document.uri, currentStates);

    if (requestImports.length === 0) {
      return;
    }

    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      this.#store.set(document.uri, markLoadingStatesUnavailable(states, "Daemon unavailable"));
      return;
    }

    this.#statusBar.setStatus("computing");
    this.#logger.debug(`Starting batch request ${requestId} with ${requestImports.length} import(s).`);

    try {
      const resultLogger = new ImportResultLogTracker(
        this.#logger.child({ component: "analysis" }),
        requestId,
      );

      const applyPartial = (partial: BatchResponse): void => {
        const nextStates = applyStreamingBatchPartial(partial, {
          requestId,
          isCurrent: (partialRequestId) => this.#freshness.isCurrent(documentKey, partialRequestId),
          requestStateIndexes,
          states: currentStates,
          isMissing: (state) => state.status === "missing",
          matchesResult: (state, result) => result.specifier === state.detected.specifier,
          applyReady: (state, result) => {
            resultLogger.logResult(result);
            return {
              detected: state.detected,
              status: "ready" as const,
              result,
            };
          },
          commit: (states) => {
            currentStates = [...states];
            this.#store.set(document.uri, currentStates);
          },
        });

        if (nextStates) {
          currentStates = [...nextStates];
        }
      };

      const response = await this.#daemon.sendBatch(
        {
          version: protocolVersion,
          request_id: requestId,
          workspace_root: workspaceRoot,
          active_document_path: document.fileName,
          imports: requestImports,
          streaming: true,
        },
        applyPartial,
      );

      if (!response) {
        this.#store.set(document.uri, markLoadingStatesUnavailable(currentStates, "No daemon response"));
        this.#statusBar.setStatus("unavailable");
        return;
      }

      if (!this.#freshness.isCurrent(documentKey, response.request_id)) {
        return;
      }

      const responseStates = applyFinalBatchResults(
        currentStates,
        response.imports,
        (specifier, reason) => resultLogger.logMissingResult(specifier, reason),
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
        await recordImportCostHistory(this.#historyStore, importCostHistoryItemsForStates(responseStates));
      } catch (error) {
        this.#logger.warn(`Import history update failed: ${error instanceof Error ? error.message : String(error)}`);
      }
      this.#statusBar.setStatus("ready");
      this.#logger.debug(`Completed batch request ${requestId}.`);
    } catch (error) {
      this.#logger.warn(`Analysis request failed: ${error instanceof Error ? error.message : String(error)}`);
      this.#store.set(document.uri, markLoadingStatesUnavailable(states, "Daemon unavailable"));
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
