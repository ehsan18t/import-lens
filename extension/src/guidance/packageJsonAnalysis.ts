import * as vscode from "vscode";
import { AnalysisFreshnessTracker } from "../analysis/freshness.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import { resolveInstalledPackage } from "../imports/resolver.js";
import { protocolVersion, type BatchResponse, type ImportRequest } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { ImportLensLogger } from "../logger.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { analysisRootForFile } from "../workspaceContext.js";
import {
  packageJsonDependencyEntries,
  packageJsonDependencySections,
  type PackageJsonDependencyEntry,
  type PackageJsonDependencySection,
} from "./packageJsonDependencies.js";
import { fetchRegistryHint, getCachedRegistryHint } from "./registryHints.js";
import type { PackageJsonDependencyHintState } from "./packageJsonState.js";

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

    const source = document.getText();
    const entries = packageJsonDependencyEntries(source);
    const sections = packageJsonDependencySections(source);

    if (entries.length === 0) {
      this.clear(document.uri);
      return;
    }

    this.#sections.set(key, sections);

    const states: PackageJsonDependencyAnalysisState[] = [];
    const requestImports: ImportRequest[] = [];
    const requestStateIndexes: number[] = [];

    for (const entry of entries) {
      const resolution = await resolveInstalledPackage(entry.name, document.fileName);

      if (!resolution.ok) {
        states.push({
          entry,
          name: entry.name,
          section: entry.section,
          status: "missing",
          message: resolution.reason === "package_not_found" ? "Package not found" : "Invalid package.json",
          registryHint: null,
        });
        continue;
      }

      const registryHint = this.registryHintForEntry(document.uri, requestId, entry, resolution.version);
      requestStateIndexes.push(states.length);
      requestImports.push({
        specifier: entry.name,
        package: entry.name,
        version: resolution.version,
        named: [],
        import_kind: "namespace",
        runtime: "component",
      });
      states.push({
        entry,
        name: entry.name,
        section: entry.section,
        status: "loading",
        registryHint,
      });
    }

    if (!this.#freshness.isCurrent(key, requestId)) {
      return;
    }

    let currentStates = states;
    this.setStates(document.uri, currentStates);

    if (requestImports.length === 0) {
      return;
    }

    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (!this.#freshness.isCurrent(key, requestId)) {
      return;
    }

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      if (!this.#freshness.isCurrent(key, requestId)) {
        return;
      }

      this.setStates(document.uri, markLoadingUnavailable(currentStates, "Daemon unavailable"));
      return;
    }

    try {
      const applyPartial = (partial: BatchResponse): void => {
        if (!this.#freshness.isCurrent(key, partial.request_id) || !partial.indexes) {
          return;
        }

        const nextStates = [...currentStates];

        partial.indexes.forEach((requestImportIndex, partialIndex) => {
          const stateIndex = requestStateIndexes[requestImportIndex];
          const state = stateIndex === undefined ? undefined : nextStates[stateIndex];
          const result = partial.imports[partialIndex];

          if (!state || state.status === "missing" || !result || result.specifier !== state.name) {
            return;
          }

          nextStates[stateIndex] = {
            ...state,
            status: "ready",
            result,
          };
        });

        currentStates = nextStates;
        this.setStates(document.uri, currentStates);
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
        this.setStates(document.uri, markLoadingUnavailable(currentStates, "No daemon response"));
        return;
      }

      if (!this.#freshness.isCurrent(key, response.request_id)) {
        return;
      }

      let responseIndex = 0;
      const nextStates = currentStates.map((state) => {
        if (state.status === "missing") {
          return state;
        }

        const result = response.imports[responseIndex++];

        if (!result || result.specifier !== state.name) {
          return {
            ...state,
            status: "unavailable" as const,
            message: "No daemon response",
          };
        }

        return {
          ...state,
          status: "ready" as const,
          result,
        };
      });

      this.setStates(document.uri, nextStates);
    } catch (error) {
      this.#logger.warn(`Package.json dependency analysis failed: ${error instanceof Error ? error.message : String(error)}`);
      this.setStates(document.uri, markLoadingUnavailable(currentStates, "Daemon unavailable"));
    }
  }

  refreshVisibleDocuments(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      this.schedule(editor.document);
    }
  }

  clear(uri: vscode.Uri): void {
    const key = uri.toString();
    this.#states.delete(key);
    this.#sections.delete(key);
    this.#freshness.forget(key);
    this.#onDidChange.fire(uri);
  }

  private registryHintForEntry(
    uri: vscode.Uri,
    requestId: number,
    entry: PackageJsonDependencyEntry,
    installedVersion: string,
  ): PackageJsonDependencyAnalysisState["registryHint"] {
    if (!getImportLensConfig().enableRegistryHints) {
      return null;
    }

    const cached = getCachedRegistryHint(this.#context, entry.name, installedVersion);

    if (cached) {
      return cached;
    }

    void fetchRegistryHint(this.#context, entry.name, {
      installedVersion,
      logger: this.#logger,
    }).then((hint) => {
      if (hint) {
        this.applyRegistryHint(uri, requestId, entry.name, hint);
      }
    });

    return null;
  }

  private applyRegistryHint(
    uri: vscode.Uri,
    requestId: number,
    packageName: string,
    hint: NonNullable<PackageJsonDependencyAnalysisState["registryHint"]>,
  ): void {
    const key = uri.toString();

    if (!this.#freshness.isCurrent(key, requestId)) {
      return;
    }

    const states = this.#states.get(key);

    if (!states) {
      return;
    }

    let changed = false;
    const nextStates = states.map((state) => {
      if (state.name !== packageName || state.registryHint) {
        return state;
      }

      changed = true;
      return { ...state, registryHint: hint };
    });

    if (changed) {
      this.setStates(uri, nextStates);
    }
  }

  private setStates(uri: vscode.Uri, states: PackageJsonDependencyAnalysisState[]): void {
    this.#states.set(uri.toString(), states);
    this.#onDidChange.fire(uri);
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

const markLoadingUnavailable = (
  states: readonly PackageJsonDependencyAnalysisState[],
  message: string,
): PackageJsonDependencyAnalysisState[] =>
  states.map((state) =>
    state.status === "loading"
      ? { ...state, status: "unavailable", message }
      : state);
