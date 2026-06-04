import * as vscode from "vscode";
import { protocolVersion } from "../ipc/protocol.js";
import { namedImportCompletionContext } from "../imports/completionContext.js";
import { resolveInstalledPackage } from "../imports/resolver.js";
import type { DaemonManager } from "../daemon/manager.js";
import { analysisRootForFile } from "../workspaceContext.js";

export class ImportMemberCompletionProvider implements vscode.CompletionItemProvider {
  readonly #daemon: DaemonManager;
  #requestId = 0;

  constructor(daemon: DaemonManager) {
    this.#daemon = daemon;
  }

  async provideCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position,
    token: vscode.CancellationToken,
  ): Promise<vscode.CompletionItem[] | undefined> {
    if (token.isCancellationRequested) {
      return undefined;
    }

    const context = namedImportCompletionContext(document.getText(), document.offsetAt(position));

    if (!context) {
      return undefined;
    }

    const resolved = await resolveInstalledPackage(context.specifier, document.fileName);

    if (!resolved.ok || token.isCancellationRequested) {
      return undefined;
    }

    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      return undefined;
    }

    const response = await this.#daemon.enumerateExports({
      type: "enumerate_exports",
      version: protocolVersion,
      request_id: ++this.#requestId,
      workspace_root: workspaceRoot,
      active_document_path: document.fileName,
      specifier: context.specifier,
      package: resolved.packageName,
      package_version: resolved.version,
    });

    if (!response || response.error || response.specifier !== context.specifier || token.isCancellationRequested) {
      return undefined;
    }

    const importedNames = new Set(context.importedNames);

    return response.exports
      .filter((exportedName) => exportedName !== "default")
      .filter((exportedName) => !importedNames.has(exportedName))
      .sort((left, right) => left.localeCompare(right))
      .map((exportedName) => {
        const item = new vscode.CompletionItem(exportedName, vscode.CompletionItemKind.Variable);
        item.detail = context.specifier;
        return item;
      });
  }
}
