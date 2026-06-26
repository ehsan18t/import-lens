import * as vscode from "vscode";
import { AnalysisFreshnessTracker } from "../analysis/freshness.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import {
  protocolVersion,
  type AnalyzePackageJsonResponse,
  type PackageJsonDependencyEntry,
  type PackageJsonDependencyAnalysisItem,
  type PackageJsonDependencySectionName,
  type PackageJsonDependencySection,
  type RegistryHint,
} from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { ImportLensLogger } from "../logger.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { analysisRootForFile } from "../workspaceContext.js";
import { markPackageJsonLoadingUnavailable, mergePackageJsonAnalysisPartial } from "./packageJsonPartial.js";
import type { PackageJsonDependencyHintState } from "./packageJsonState.js";
import { fetchRegistryHint, getCachedRegistryHint } from "./registryHints.js";

export interface PackageJsonDependencyAnalysisState extends PackageJsonDependencyHintState {
  entry: PackageJsonDependencyEntry;
  message?: string;
}

export class PackageJsonAnalysisController implements vscode.Disposable {
  readonly #context: vscode.ExtensionContext;
  readonly #daemon: DaemonManager;
  readonly #logger: ImportLensLogger;
  readonly #timers = new Map<string, NodeJS.Timeout>();
  readonly #freshness = new AnalysisFreshnessTracker();
  readonly #states = new Map<string, PackageJsonDependencyAnalysisState[]>();
  readonly #sections = new Map<string, PackageJsonDependencySection[]>();
  readonly #onDidChange = new vscode.EventEmitter<vscode.Uri>();

  readonly onDidChange: vscode.Event<vscode.Uri> = this.#onDidChange.event;

  constructor(
    context: vscode.ExtensionContext,
    daemon: DaemonManager,
    logger: ImportLensLogger,
  ) {
    this.#context = context;
    this.#daemon = daemon;
    this.#logger = logger;

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
    const key = document.uri.toString();
    const requestId = this.#freshness.begin(key, nextIpcRequestId());

    if (!config.enabled || !isPackageJsonDocument(document)) {
      this.clear(document.uri);
      return;
    }

    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (!this.#freshness.isCurrent(key, requestId)) {
      return;
    }

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      this.clear(document.uri);
      return;
    }

    try {
      const response = await this.#daemon.analyzePackageJson({
        type: "analyze_package_json",
        version: protocolVersion,
        request_id: requestId,
        workspace_root: workspaceRoot,
        active_document_path: document.fileName,
        source: document.getText(),
        streaming: true,
        include_registry_hints: false,
      }, (partial) => this.handlePackageJsonPartial(document.uri, key, partial));

      if (!response) {
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
      const mergedStates = mergePackageJsonAnalysisPartial(this.#states.get(key) ?? [], response);
      const states = this.applyCachedRegistryHints(mergedStates);
      this.setStates(document.uri, states);
      this.queueRegistryRefreshes(document.uri, states);
    } catch (error) {
      this.#logger.warn(`Package.json dependency analysis failed: ${error instanceof Error ? error.message : String(error)}`);
      this.markLoadingUnavailable(
        document.uri,
        error instanceof Error ? error.message : "Daemon unavailable",
      );
    }
  }

  refreshVisibleDocuments(): void {
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
    const targets = states.filter((state) =>
      (!options.section || state.section === options.section) &&
      (!options.packageName ||
        (state.name === options.packageName && state.installedVersion === options.installedVersion)));

    if (targets.length === 0) {
      return;
    }

    await Promise.all(targets.map(async (state) => {
      const hint = await fetchRegistryHint(this.#context.globalState, state.name, {
        installedVersion: state.installedVersion,
        forceRefresh: true,
        logger: this.#logger,
      });
      this.updateRegistryHint(uri, state.name, state.installedVersion, hint);
    }));
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

    const mergedStates = mergePackageJsonAnalysisPartial(this.#states.get(key) ?? [], partial);
    const states = this.applyCachedRegistryHints(mergedStates);
    this.setStates(uri, states);
    this.queueRegistryRefreshes(uri, states, partial.indexes);
  }

  private applyCachedRegistryHints(
    states: readonly PackageJsonDependencyAnalysisItem[],
  ): PackageJsonDependencyAnalysisState[] {
    if (!getImportLensConfig().enableRegistryHints) {
      return states.map((state) => ({ ...state, registryHint: undefined }));
    }

    return states.map((state) => {
      const cached = getCachedRegistryHint(this.#context.globalState, state.name, state.installedVersion, {
        allowStale: true,
      });

      return {
        ...state,
        registryHint: newerRegistryHint(state.registryHint, cached),
      };
    });
  }

  private queueRegistryRefreshes(
    uri: vscode.Uri,
    states: readonly PackageJsonDependencyAnalysisState[],
    indexes?: readonly number[],
  ): void {
    if (!getImportLensConfig().enableRegistryHints) {
      return;
    }

    const selectedStates = indexes
      ? indexes.map((index) => states[index]).filter((state): state is PackageJsonDependencyAnalysisState => !!state)
      : states;
    const seen = new Set<string>();

    for (const state of selectedStates) {
      const key = `${state.name}\n${state.installedVersion ?? ""}`;

      if (seen.has(key)) {
        continue;
      }

      seen.add(key);
      void fetchRegistryHint(this.#context.globalState, state.name, {
        installedVersion: state.installedVersion,
        logger: this.#logger,
      })
        .then((hint) => this.updateRegistryHint(uri, state.name, state.installedVersion, hint))
        .catch((error: unknown) => {
          this.#logger.debug(`Registry hint refresh failed for ${state.name}: ${error instanceof Error ? error.message : String(error)}`);
        });
    }
  }

  private updateRegistryHint(
    uri: vscode.Uri,
    packageName: string,
    installedVersion: string | undefined,
    hint: RegistryHint | null,
  ): void {
    const key = uri.toString();
    const states = this.#states.get(key);

    if (!states) {
      return;
    }

    let changed = false;
    const nextStates = states.map((state) => {
      if (state.name !== packageName || state.installedVersion !== installedVersion) {
        return state;
      }

      const registryHint = newerRegistryHint(state.registryHint, hint);

      if (registryHint === state.registryHint) {
        return state;
      }

      changed = true;
      return {
        ...state,
        registryHint,
      };
    });

    if (changed) {
      this.setStates(uri, nextStates);
    }
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
    const key = document.uri.toString();
    const timer = this.#timers.get(key);

    if (timer) {
      clearTimeout(timer);
      this.#timers.delete(key);
    }

    this.clear(document.uri);
  }

  dispose(): void {
    for (const timer of this.#timers.values()) {
      clearTimeout(timer);
    }

    this.#timers.clear();
    this.#freshness.clear();
    this.#onDidChange.dispose();
  }
}

const isPackageJsonDocument = (document: vscode.TextDocument): boolean =>
  document.uri.scheme === "file" && isPackageJsonPath(document.fileName);

const newerRegistryHint = (
  current: RegistryHint | null | undefined,
  incoming: RegistryHint | null | undefined,
): RegistryHint | null | undefined => {
  if (incoming === undefined || incoming === null) {
    return current;
  }

  if (current === undefined || current === null) {
    return incoming;
  }

  return (current.fetchedAt ?? 0) > (incoming.fetchedAt ?? 0) ? current : incoming;
};
