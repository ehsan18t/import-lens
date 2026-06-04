import * as vscode from "vscode";
import { AnalysisFreshnessTracker } from "./analysis/freshness.js";
import { createImportRequest } from "./analysis/request.js";
import { markLoadingStatesUnavailable } from "./analysis/status.js";
import type { AnalysisStore, ImportAnalysisState } from "./analysis/state.js";
import { getImportLensConfig } from "./config.js";
import type { DaemonManager } from "./daemon/manager.js";
import { extractRuntimeImports } from "./imports/parser.js";
import { resolveInstalledPackage } from "./imports/resolver.js";
import { supportedLanguageIds } from "./languages.js";
import type { ImportLensLogger } from "./logger.js";
import type { StatusBarController } from "./ui/statusbar.js";
import { protocolVersion, type BatchResponse } from "./ipc/protocol.js";
import { analysisRootForFile } from "./workspaceContext.js";

export class DocumentAnalysisController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #daemon: DaemonManager;
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
    const requestId = this.#freshness.begin(documentKey);

    if (!config.enabled || !supportedLanguageIds.has(document.languageId)) {
      this.#store.clear(document.uri);
      return;
    }

    const imports = extractRuntimeImports(document.fileName, document.getText());

    if (imports.length === 0) {
      this.#store.clear(document.uri);
      return;
    }

    const states: ImportAnalysisState[] = [];
    const requestImports = [];
    const requestStateIndexes: number[] = [];

    for (const detected of imports) {
      const resolution = await resolveInstalledPackage(detected.specifier, document.fileName);

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

    try {
      const applyPartial = (partial: BatchResponse): void => {
        if (!this.#freshness.isCurrent(documentKey, partial.request_id) || !partial.indexes) {
          return;
        }

        const nextStates = [...currentStates];

        partial.indexes.forEach((requestImportIndex, partialIndex) => {
          const stateIndex = requestStateIndexes[requestImportIndex];
          const state = stateIndex === undefined ? undefined : nextStates[stateIndex];
          const result = partial.imports[partialIndex];

          if (!state || state.status === "missing" || !result || result.specifier !== state.detected.specifier) {
            return;
          }

          if (result.error) {
            this.#logger.warn(`${result.specifier}: ${result.error}`);
          }

          nextStates[stateIndex] = {
            detected: state.detected,
            status: "ready",
            result,
          };
        });

        currentStates = nextStates;
        this.#store.set(document.uri, currentStates);
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

      if (!response || !this.#freshness.isCurrent(documentKey, response.request_id)) {
        return;
      }

      let responseIndex = 0;

      const nextStates = states.map((state) => {
        if (state.status === "missing") {
          return state;
        }

        const result = response.imports[responseIndex++];

        if (!result || result.specifier !== state.detected.specifier) {
          return state;
        }

        if (result.error) {
          this.#logger.warn(`${result.specifier}: ${result.error}`);
        }

        return {
          detected: state.detected,
          status: "ready" as const,
          result,
        };
      });

      this.#store.set(document.uri, nextStates);
      this.#statusBar.setStatus("ready");
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
