import path from "node:path";
import * as vscode from "vscode";
import { packageJsonDependencyEntries, type PackageJsonDependencyEntry } from "../guidance/packageJsonDependencies.js";
import { registryHintForPackage } from "../guidance/registryHints.js";
import type { DaemonManager } from "../daemon/manager.js";
import { resolveInstalledPackage } from "../imports/resolver.js";
import { protocolVersion, type ImportRequest, type ImportResult } from "../ipc/protocol.js";
import { getImportLensConfig } from "../config.js";
import { formatBytes } from "./format.js";

let packageJsonLensRequestId = Date.now();

export class PackageJsonDependencyCodeLensProvider implements vscode.CodeLensProvider, vscode.Disposable {
  readonly #context: vscode.ExtensionContext;
  readonly #daemon: DaemonManager;
  readonly #onDidChangeCodeLenses = new vscode.EventEmitter<void>();

  readonly onDidChangeCodeLenses: vscode.Event<void> = this.#onDidChangeCodeLenses.event;

  constructor(context: vscode.ExtensionContext, daemon: DaemonManager) {
    this.#context = context;
    this.#daemon = daemon;
  }

  async provideCodeLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    const config = getImportLensConfig();

    if (!config.enabled || path.basename(document.fileName) !== "package.json") {
      return [];
    }

    const entries = packageJsonDependencyEntries(document.getText());
    if (entries.length === 0) {
      return [];
    }

    const requestEntries = await dependencyRequestEntries(entries, document.fileName);
    const workspaceRoot = path.dirname(document.fileName);

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      return entries.map((entry) => this.lensForEntry(entry, "ImportLens: daemon unavailable"));
    }

    const response = requestEntries.length > 0
      ? await this.#daemon.sendBatch({
        version: protocolVersion,
        request_id: ++packageJsonLensRequestId,
        workspace_root: workspaceRoot,
        active_document_path: document.fileName,
        imports: requestEntries.map((entry) => entry.request),
      })
      : null;
    const resultByEntry = new Map<PackageJsonDependencyEntry, ImportResult>();
    response?.imports.forEach((result, index) => {
      const requestEntry = requestEntries[index];
      if (requestEntry) {
        resultByEntry.set(requestEntry.entry, result);
      }
    });

    return Promise.all(entries.map(async (entry) => {
      const result = resultByEntry.get(entry);
      const registryHint = config.enableRegistryHints
        ? await registryHintForPackage(this.#context, entry.name)
        : null;

      let suffix = "";
      if (registryHint) {
        if (registryHint.deprecated) {
          suffix = " · deprecated";
        } else if (registryHint.latestVersion) {
          const cleanVersion = entry.version.replace(/^[^\d]+/, "");
          if (cleanVersion !== registryHint.latestVersion) {
            suffix = ` · latest ${registryHint.latestVersion}`;
          }
        }
      }

      const title = result && !result.error
        ? `ImportLens: ${formatBytes(result.brotli_bytes)} br${suffix}`
        : `ImportLens: size unavailable${suffix}`;
      return this.lensForEntry(entry, title);
    }));
  }

  refresh(): void {
    this.#onDidChangeCodeLenses.fire();
  }

  dispose(): void {
    this.#onDidChangeCodeLenses.dispose();
  }

  private lensForEntry(
    entry: ReturnType<typeof packageJsonDependencyEntries>[number],
    title: string,
  ): vscode.CodeLens {
    return new vscode.CodeLens(
      new vscode.Range(
        entry.range.start.line,
        entry.range.start.character,
        entry.range.end.line,
        entry.range.end.character,
      ),
      {
        title,
        command: "importLens.compareImports",
        arguments: [entry.name],
      },
    );
  }
}

interface PackageJsonDependencyRequestEntry {
  entry: PackageJsonDependencyEntry;
  request: ImportRequest;
}

const dependencyRequestEntries = async (
  entries: readonly PackageJsonDependencyEntry[],
  packageJsonPath: string,
): Promise<PackageJsonDependencyRequestEntry[]> => {
  const requests: PackageJsonDependencyRequestEntry[] = [];

  for (const entry of entries) {
    const resolution = await resolveInstalledPackage(entry.name, packageJsonPath);

    if (!resolution.ok) {
      continue;
    }

    requests.push({
      entry,
      request: {
        specifier: entry.name,
        package: entry.name,
        version: resolution.version,
        named: [],
        import_kind: "namespace",
        runtime: "component",
      },
    });
  }

  return requests;
};
