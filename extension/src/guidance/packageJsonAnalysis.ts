import * as vscode from "vscode";
import { AnalysisFreshnessTracker } from "../analysis/freshness.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import {
  protocolVersion,
  type PackageJsonDependencyEntry,
  type PackageJsonDependencySectionName,
  type PackageJsonDependencySection,
} from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { ImportLensLogger } from "../logger.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { analysisRootForFile } from "../workspaceContext.js";
import type { PackageJsonDependencyHintState } from "./packageJsonState.js";

export interface PackageJsonDependencyAnalysisState extends PackageJsonDependencyHintState {
  entry: PackageJsonDependencyEntry;
  message?: string;
}

export class PackageJsonAnalysisController implements vscode.Disposable {
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
        include_registry_hints: config.enableRegistryHints,
      });

      if (!response) {
        this.clear(document.uri);
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
      this.setStates(document.uri, response.states);
    } catch (error) {
      this.#logger.warn(`Package.json dependency analysis failed: ${error instanceof Error ? error.message : String(error)}`);
      this.clear(document.uri);
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
    const document = await vscode.workspace.openTextDocument(uri);
    const requestId = this.#freshness.begin(uri.toString(), nextIpcRequestId());
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      return;
    }

    const response = await this.#daemon.analyzePackageJson({
      type: "analyze_package_json",
      version: protocolVersion,
      request_id: requestId,
      workspace_root: workspaceRoot,
      active_document_path: document.fileName,
      source: document.getText(),
      include_registry_hints: true,
      force_registry_refresh: true,
      refresh_section: options.section,
    });

    if (!response || !this.#freshness.isCurrent(uri.toString(), response.request_id) || response.error) {
      return;
    }

    const states = options.packageName
      ? mergeSinglePackageRegistryHint(
        this.#states.get(uri.toString()) ?? [],
        response.states,
        options.packageName,
        options.installedVersion,
      )
      : response.states;

    this.#sections.set(uri.toString(), response.sections);
    this.setStates(uri, states);
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

const mergeSinglePackageRegistryHint = (
  currentStates: readonly PackageJsonDependencyAnalysisState[],
  refreshedStates: readonly PackageJsonDependencyAnalysisState[],
  packageName: string,
  installedVersion?: string,
): PackageJsonDependencyAnalysisState[] => {
  const refreshed = refreshedStates.find((state) =>
    state.name === packageName && state.installedVersion === installedVersion);

  if (!refreshed) {
    return [...currentStates];
  }

  return currentStates.map((state) =>
    state.name === packageName && state.installedVersion === installedVersion
      ? { ...state, registryHint: refreshed.registryHint ?? null }
      : state);
};
