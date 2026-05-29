import * as vscode from "vscode";
import { createImportRequest } from "./analysis/request.js";
import type { AnalysisStore, ImportAnalysisState } from "./analysis/state.js";
import { getImportLensConfig } from "./config.js";
import type { DaemonManager } from "./daemon/manager.js";
import { extractRuntimeImports } from "./imports/parser.js";
import { resolveInstalledPackage } from "./imports/resolver.js";
import { supportedLanguageIds } from "./languages.js";
import type { ImportLensLogger } from "./logger.js";
import type { StatusBarController } from "./ui/statusbar.js";
import { protocolVersion } from "./ipc/protocol.js";

export class DocumentAnalysisController implements vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #daemon: DaemonManager;
  readonly #logger: ImportLensLogger;
  readonly #statusBar: StatusBarController;
  readonly #timers = new Map<string, NodeJS.Timeout>();
  #requestId = 0;
  #latestRequestId = 0;

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
      requestImports.push(createImportRequest(detected, resolution.version));
    }

    this.#store.set(document.uri, states);

    if (requestImports.length === 0 || this.#daemon.state !== "ready") {
      return;
    }

    const requestId = ++this.#requestId;
    this.#latestRequestId = requestId;
    this.#statusBar.setStatus("computing");

    try {
      const response = await this.#daemon.sendBatch({
        version: protocolVersion,
        request_id: requestId,
        active_document_path: document.fileName,
        imports: requestImports,
      });

      if (!response || response.request_id !== this.#latestRequestId) {
        return;
      }

      const nextStates = states.map((state) => {
        const result = response.imports.find((item) => item.specifier === state.detected.specifier);

        if (!result) {
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
      this.#statusBar.setStatus("unavailable");
    }
  }

  dispose(): void {
    for (const timer of this.#timers.values()) {
      clearTimeout(timer);
    }

    this.#timers.clear();
  }
}
