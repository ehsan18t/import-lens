import * as vscode from "vscode";
import { DebouncedDocumentScheduler } from "../analysis/debouncedDocumentScheduler.js";
import { AnalysisFreshnessTracker } from "../analysis/freshness.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import {
  type AnalyzePackageJsonResponse,
  type PackageJsonDependencyEntry,
  type PackageJsonDependencySection,
  type PackageJsonDependencySectionName,
  protocolVersion,
} from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { ImportLensLogger } from "../logger.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { analysisRootForFile } from "../workspaceContext.js";
import { AnalyzedContentTracker } from "./analyzedContentTracker.js";
import {
  markPackageJsonLoadingUnavailable,
  mergePackageJsonAnalysisPartial,
} from "./packageJsonPartial.js";
import type { PackageJsonDependencyHintState } from "./packageJsonState.js";
import { RegistryHintRefresher, registryTargetsForStates } from "./registryRefresh.js";

export interface PackageJsonDependencyAnalysisState extends PackageJsonDependencyHintState {
  entry: PackageJsonDependencyEntry;
  message?: string;
}

export class PackageJsonAnalysisController implements vscode.Disposable {
  readonly #daemon: DaemonManager;
  readonly #logger: ImportLensLogger;
  readonly #scheduler = new DebouncedDocumentScheduler();
  readonly #freshness = new AnalysisFreshnessTracker();
  readonly #states = new Map<string, PackageJsonDependencyAnalysisState[]>();
  readonly #sections = new Map<string, PackageJsonDependencySection[]>();
  readonly #analyzedContent = new AnalyzedContentTracker();
  readonly #onDidChange = new vscode.EventEmitter<vscode.Uri>();
  readonly #registryRefresher: RegistryHintRefresher<
    vscode.Uri,
    PackageJsonDependencyAnalysisState
  >;

  readonly onDidChange: vscode.Event<vscode.Uri> = this.#onDidChange.event;

  constructor(context: vscode.ExtensionContext, daemon: DaemonManager, logger: ImportLensLogger) {
    this.#daemon = daemon;
    this.#logger = logger;
    this.#registryRefresher = new RegistryHintRefresher(
      daemon,
      {
        keyFor: (uri) => uri.toString(),
        getStates: (uri) => this.#states.get(uri.toString()),
        setStates: (uri, states) => this.setStates(uri, states),
      },
      logger,
      () => getImportLensConfig().verboseRegistryLogging,
    );

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

    for (const document of vscode.workspace.textDocuments) {
      this.schedule(document);
    }
  }

  get(uri: vscode.Uri): PackageJsonDependencyAnalysisState[] {
    return this.#states.get(uri.toString()) ?? [];
  }

  sections(uri: vscode.Uri): PackageJsonDependencySection[] {
    return this.#sections.get(uri.toString()) ?? [];
  }

  schedule(document: vscode.TextDocument): void {
    if (!isPackageJsonDocument(document)) {
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
    const key = document.uri.toString();
    const requestId = this.#freshness.begin(key, nextIpcRequestId());

    if (!config.enabled || !isPackageJsonDocument(document)) {
      this.clear(document.uri);
      return;
    }

    // Skip redundant re-analysis on passive triggers (tab focus, re-open) when
    // the document text is identical to the last successful analysis. Explicit
    // refreshes call refreshVisibleDocuments(), which forgets this first.
    const currentText = document.getText();
    if (this.#analyzedContent.isUnchanged(key, currentText)) {
      return;
    }

    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (!this.#freshness.isCurrent(key, requestId)) {
      return;
    }

    if (this.#daemon.state !== "ready" && (await this.#daemon.start(workspaceRoot)) !== "ready") {
      this.clear(document.uri);
      return;
    }

    try {
      const response = await this.#daemon.analyzePackageJson(
        {
          type: "analyze_package_json",
          version: protocolVersion,
          request_id: requestId,
          workspace_root: workspaceRoot,
          active_document_path: document.fileName,
          // Reuse the text captured for the unchanged-content guard so the
          // recorded key always matches the text actually analyzed.
          source: currentText,
          streaming: true,
          include_registry_hints: config.enableRegistryHints,
          registry_hint_mode: config.enableRegistryHints ? "cached" : "off",
        },
        (partial) => this.handlePackageJsonPartial(document.uri, key, partial),
      );

      if (!response) {
        if (!this.#freshness.isCurrent(key, requestId)) {
          return;
        }
        this.markLoadingUnavailable(document.uri, "Daemon unavailable");
        return;
      }

      if (!this.#freshness.isCurrent(key, response.request_id)) {
        return;
      }

      if (response.error || response.states.length === 0) {
        this.clear(document.uri);
        return;
      }

      this.#sections.set(key, response.sections);
      const states = mergePackageJsonAnalysisPartial(this.#states.get(key) ?? [], response);
      this.setStates(document.uri, states);
      this.queueRegistryRefreshes(document.uri, states);
      this.#analyzedContent.record(key, currentText);
    } catch (error) {
      this.#logger.warn(
        `Package.json dependency analysis failed: ${error instanceof Error ? error.message : String(error)}`,
      );
      if (!this.#freshness.isCurrent(key, requestId)) {
        return;
      }
      this.markLoadingUnavailable(
        document.uri,
        error instanceof Error ? error.message : "Daemon unavailable",
      );
    }
  }

  refreshVisibleDocuments(): void {
    // Explicit refresh (config change, daemon restart, cache clear, node_modules
    // watcher) must bypass the unchanged-content guard so re-analysis runs. Forget
    // ALL tracked docs, not just visible ones — a background package.json tab would
    // otherwise stay stale (same text) until edited when next focused.
    this.#analyzedContent.forgetAll();
    for (const editor of vscode.window.visibleTextEditors) {
      this.schedule(editor.document);
    }
  }

  async refreshRegistryHint(
    uri: vscode.Uri,
    packageName: string,
    installedVersion?: string,
  ): Promise<void> {
    if (!getImportLensConfig().enableRegistryHints) {
      return;
    }

    await this.refreshRegistryHintsForUri(uri, {
      packageName,
      installedVersion,
    });
  }

  async refreshRegistryHints(
    uri: vscode.Uri,
    section?: PackageJsonDependencySectionName,
  ): Promise<void> {
    if (!getImportLensConfig().enableRegistryHints) {
      return;
    }

    await this.refreshRegistryHintsForUri(uri, { section });
  }

  clear(uri: vscode.Uri): void {
    const key = uri.toString();
    this.#states.delete(key);
    this.#sections.delete(key);
    this.#freshness.forget(key);
    this.#analyzedContent.forget(key);
    this.#registryRefresher.forget(uri);
    this.#onDidChange.fire(uri);
  }

  private setStates(uri: vscode.Uri, states: PackageJsonDependencyAnalysisState[]): void {
    this.#states.set(uri.toString(), states);
    this.#onDidChange.fire(uri);
  }

  private async refreshRegistryHintsForUri(
    uri: vscode.Uri,
    options: {
      section?: PackageJsonDependencySectionName;
      packageName?: string;
      installedVersion?: string;
    } = {},
  ): Promise<void> {
    const states = this.#states.get(uri.toString()) ?? [];
    const targets = states.filter(
      (state) =>
        (!options.section || state.section === options.section) &&
        (!options.packageName ||
          (state.name === options.packageName &&
            state.installedVersion === options.installedVersion)),
    );

    if (targets.length === 0) {
      return;
    }

    await this.#registryRefresher.refresh(uri, registryTargetsForStates(targets), "force_refresh");
  }

  private handlePackageJsonPartial(
    uri: vscode.Uri,
    key: string,
    partial: AnalyzePackageJsonResponse,
  ): void {
    if (!this.#freshness.isCurrent(key, partial.request_id) || partial.error) {
      return;
    }

    if (partial.sections.length > 0) {
      this.#sections.set(key, partial.sections);
    }

    const states = mergePackageJsonAnalysisPartial(this.#states.get(key) ?? [], partial);
    this.setStates(uri, states);
    // Registry refreshes are queued once from analyze() after the final response,
    // not per streaming partial — otherwise every per-package partial fires its
    // own refresh IPC (~one batch + one-per-package + a final batch per analysis).
  }

  private queueRegistryRefreshes(
    uri: vscode.Uri,
    states: readonly PackageJsonDependencyAnalysisState[],
  ): void {
    if (!getImportLensConfig().enableRegistryHints) {
      return;
    }

    const targets = registryTargetsForStates(states);

    if (targets.length === 0) {
      return;
    }

    void this.#registryRefresher.refresh(uri, targets, "refresh_stale");
  }

  private markLoadingUnavailable(uri: vscode.Uri, message: string): void {
    const states = this.#states.get(uri.toString()) ?? [];

    if (states.length === 0) {
      this.clear(uri);
      return;
    }

    this.setStates(uri, markPackageJsonLoadingUnavailable(states, message));
  }

  private disposeDocument(document: vscode.TextDocument): void {
    this.#scheduler.cancel(document.uri.toString());
    this.clear(document.uri);
  }

  dispose(): void {
    this.#scheduler.dispose();
    this.#freshness.clear();
    this.#onDidChange.dispose();
  }
}

const isPackageJsonDocument = (document: vscode.TextDocument): boolean =>
  document.uri.scheme === "file" && isPackageJsonPath(document.fileName);
