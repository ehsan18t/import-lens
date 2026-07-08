import * as vscode from "vscode";
import { DebouncedDocumentScheduler } from "../analysis/debouncedDocumentScheduler.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import {
  type AnalyzePackageJsonResponse,
  type PackageJsonDependencyEntry,
  type PackageJsonDependencySection,
  type PackageJsonDependencySectionName,
  protocolVersion,
} from "../ipc/protocol.js";
import type { ImportLensLogger } from "../logger.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { analysisRootForFile } from "../workspaceContext.js";
import {
  markPackageJsonLoadingUnavailable,
  mergePackageJsonAnalysisPartial,
} from "./packageJsonPartial.js";
import { PackageJsonRequestLifecycle } from "./packageJsonRequestLifecycle.js";
import type { PackageJsonDependencyHintState } from "./packageJsonState.js";
import { RegistryHintRefresher, registryTargetsForStates } from "./registryRefresh.js";

export interface PackageJsonDependencyAnalysisState extends PackageJsonDependencyHintState {
  entry: PackageJsonDependencyEntry;
  message?: string;
}

interface PackageJsonRequestTiming {
  documentPath: string;
  startedAt: number;
  firstPartialLogged: boolean;
}

type PackageJsonScheduleSource =
  | "active_editor"
  | "change"
  | "direct"
  | "initial_text_document"
  | "open"
  | "refresh_visible";

export class PackageJsonAnalysisController implements vscode.Disposable {
  readonly #daemon: DaemonManager;
  readonly #logger: ImportLensLogger;
  readonly #scheduler = new DebouncedDocumentScheduler();
  readonly #lifecycle = new PackageJsonRequestLifecycle();
  readonly #states = new Map<string, PackageJsonDependencyAnalysisState[]>();
  readonly #sections = new Map<string, PackageJsonDependencySection[]>();
  readonly #scheduledAt = new Map<string, number>();
  readonly #requestTimings = new Map<number, PackageJsonRequestTiming>();
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

    const initialPackageJsonDocuments =
      vscode.workspace.textDocuments.filter(isPackageJsonDocument).length;
    const activeDocumentPath = vscode.window.activeTextEditor?.document.uri.fsPath ?? "none";
    this.#logger.debug(
      `Package.json analysis controller initialized (text_documents=${vscode.workspace.textDocuments.length}, package_json_documents=${initialPackageJsonDocuments}, active_editor=${activeDocumentPath}).`,
    );

    context.subscriptions.push(
      vscode.workspace.onDidChangeTextDocument((event) => this.schedule(event.document, "change")),
      vscode.workspace.onDidOpenTextDocument((document) => this.schedule(document, "open")),
      vscode.workspace.onDidCloseTextDocument((document) => this.disposeDocument(document)),
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor) {
          this.schedule(editor.document, "active_editor");
        }
      }),
    );

    for (const document of vscode.workspace.textDocuments) {
      this.schedule(document, "initial_text_document");
    }
  }

  get(uri: vscode.Uri): PackageJsonDependencyAnalysisState[] {
    return this.#states.get(uri.toString()) ?? [];
  }

  sections(uri: vscode.Uri): PackageJsonDependencySection[] {
    return this.#sections.get(uri.toString()) ?? [];
  }

  schedule(document: vscode.TextDocument, source: PackageJsonScheduleSource = "direct"): void {
    if (!isPackageJsonDocument(document)) {
      return;
    }

    const key = document.uri.toString();
    const debounceMs = getImportLensConfig().debounceMs;
    const scheduledAt = Date.now();
    const stateCount = this.#states.get(key)?.length ?? 0;
    const sectionCount = this.#sections.get(key)?.length ?? 0;
    this.#scheduledAt.set(key, scheduledAt);
    this.#logger.debug(
      `Package.json analysis scheduled (${source}) for ${document.uri.fsPath} (debounce=${debounceMs}ms, daemon=${this.#daemon.state}, version=${document.version}, states=${stateCount}, sections=${sectionCount}).`,
    );
    this.#scheduler.schedule(key, debounceMs, () => {
      const elapsedMs = Date.now() - (this.#scheduledAt.get(key) ?? scheduledAt);
      this.#logger.debug(
        `Package.json debounce fired (${source}) for ${document.uri.fsPath} after ${elapsedMs}ms.`,
      );
      void this.analyze(document);
    });
  }

  async analyze(document: vscode.TextDocument): Promise<void> {
    const config = getImportLensConfig();
    const key = document.uri.toString();
    const analysisStartedAt = Date.now();

    if (!config.enabled || !isPackageJsonDocument(document)) {
      this.#scheduledAt.delete(key);
      this.clear(document.uri);
      return;
    }

    // Skip redundant re-analysis on passive triggers (tab focus, re-open) when
    // the document text is already covered by a sent request. Explicit refreshes
    // call refreshVisibleDocuments(), which forgets this first.
    const currentText = document.getText();
    if (this.reuseUnchangedPackageJsonAnalysis(document, key, currentText)) {
      return;
    }

    const requestId = this.#lifecycle.begin(key, currentText);
    this.#logger.debug(
      `Package.json request ${requestId} preparing for ${document.uri.fsPath} (${currentText.length} chars).`,
    );

    try {
      const workspaceRoot = await this.resolveWorkspaceRootForRequest(document, requestId);

      if (!this.#lifecycle.isCurrent(key, requestId)) {
        return;
      }

      if (!(await this.ensureDaemonReadyForRequest(workspaceRoot, requestId, key, document.uri))) {
        return;
      }

      this.#requestTimings.set(requestId, {
        documentPath: document.uri.fsPath,
        startedAt: Date.now(),
        firstPartialLogged: false,
      });
      this.#logger.debug(
        `Package.json request ${requestId} sending after ${
          Date.now() - analysisStartedAt
        }ms from debounce fire.`,
      );
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
        this.logPackageJsonResponseTiming(requestId, "no response");
        if (!this.#lifecycle.isCurrent(key, requestId)) {
          return;
        }
        this.#lifecycle.fail(key);
        this.markLoadingUnavailable(document.uri, "Daemon unavailable");
        return;
      }

      if (!this.#lifecycle.isCurrent(key, response.request_id)) {
        return;
      }

      this.logPackageJsonResponseTiming(
        response.request_id,
        `final response (states=${response.states.length}, sections=${response.sections.length})`,
      );

      if (response.error || response.states.length === 0) {
        this.#lifecycle.fail(key);
        this.clear(document.uri);
        return;
      }

      this.#sections.set(key, response.sections);
      const states = mergePackageJsonAnalysisPartial(this.#states.get(key) ?? [], response);
      this.setStates(document.uri, states);
      this.queueRegistryRefreshes(document.uri, states);
    } catch (error) {
      this.#logger.warn(
        `Package.json dependency analysis failed: ${error instanceof Error ? error.message : String(error)}`,
      );
      if (!this.#lifecycle.isCurrent(key, requestId)) {
        return;
      }
      this.#lifecycle.fail(key);
      this.markLoadingUnavailable(
        document.uri,
        error instanceof Error ? error.message : "Daemon unavailable",
      );
    } finally {
      this.#requestTimings.delete(requestId);
      this.#scheduledAt.delete(key);
    }
  }

  refreshVisibleDocuments(): void {
    // Explicit refresh (config change, daemon restart, cache clear, node_modules
    // watcher) must bypass the unchanged-content guard so re-analysis runs. Forget
    // ALL tracked docs, not just visible ones — a background package.json tab would
    // otherwise stay stale (same text) until edited when next focused.
    this.#lifecycle.supersedeAll();
    for (const editor of vscode.window.visibleTextEditors) {
      this.schedule(editor.document, "refresh_visible");
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
    this.#lifecycle.forget(key);
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
    if (!this.#lifecycle.isCurrent(key, partial.request_id) || partial.error) {
      return;
    }

    const timing = this.#requestTimings.get(partial.request_id);
    if (timing && !timing.firstPartialLogged) {
      timing.firstPartialLogged = true;
      this.#logger.debug(
        `Package.json request ${partial.request_id} first partial after ${
          Date.now() - timing.startedAt
        }ms for ${timing.documentPath} (states=${partial.states.length}, indexes=${
          partial.indexes?.length ?? 0
        }).`,
      );
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

  private reuseUnchangedPackageJsonAnalysis(
    document: vscode.TextDocument,
    key: string,
    currentText: string,
  ): boolean {
    if (!this.#lifecycle.shouldSkipUnchanged(key, currentText)) {
      return false;
    }

    const stateCount = this.#states.get(key)?.length ?? 0;
    const sectionCount = this.#sections.get(key)?.length ?? 0;
    if (stateCount > 0 || sectionCount > 0) {
      this.#logger.debug(
        `Package.json analysis skipped unchanged content for ${document.uri.fsPath}; refreshing cached UI (states=${stateCount}, sections=${sectionCount}).`,
      );
      this.#onDidChange.fire(document.uri);
      this.#scheduledAt.delete(key);
      return true;
    }

    this.#logger.debug(
      `Package.json unchanged guard had no cached UI state for ${document.uri.fsPath}; forcing analysis.`,
    );
    this.#lifecycle.fail(key);
    return false;
  }

  private async resolveWorkspaceRootForRequest(
    document: vscode.TextDocument,
    requestId: number,
  ): Promise<string> {
    const rootStartedAt = Date.now();
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);
    this.#logger.debug(
      `Package.json request ${requestId} resolved analysis root in ${
        Date.now() - rootStartedAt
      }ms: ${workspaceRoot}.`,
    );
    return workspaceRoot;
  }

  private async ensureDaemonReadyForRequest(
    workspaceRoot: string,
    requestId: number,
    key: string,
    uri: vscode.Uri,
  ): Promise<boolean> {
    if (this.#daemon.state === "ready") {
      this.#logger.debug(`Package.json request ${requestId} found daemon already ready.`);
      return true;
    }

    const daemonStartedAt = Date.now();
    const daemonState = await this.#daemon.start(workspaceRoot);
    this.#logger.debug(
      `Package.json request ${requestId} daemon start gate completed in ${
        Date.now() - daemonStartedAt
      }ms with state ${daemonState}.`,
    );

    if (daemonState === "ready") {
      return true;
    }

    if (!this.#lifecycle.isCurrent(key, requestId)) {
      return false;
    }

    this.#lifecycle.fail(key);
    this.clear(uri);
    return false;
  }

  private logPackageJsonResponseTiming(requestId: number, label: string): void {
    const timing = this.#requestTimings.get(requestId);
    if (!timing) {
      return;
    }

    this.#logger.debug(
      `Package.json request ${requestId} ${label} after ${
        Date.now() - timing.startedAt
      }ms for ${timing.documentPath}.`,
    );
  }

  dispose(): void {
    this.#scheduler.dispose();
    this.#lifecycle.supersedeAll();
    this.#onDidChange.dispose();
  }
}

const isPackageJsonDocument = (document: vscode.TextDocument): boolean =>
  document.uri.scheme === "file" && isPackageJsonPath(document.fileName);
