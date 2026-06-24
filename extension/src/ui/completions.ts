import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import { protocolVersion } from "../ipc/protocol.js";
import type { DaemonManager } from "../daemon/manager.js";
import { analysisRootForFile } from "../workspaceContext.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import { shouldOfferImportCompletions } from "./displayGuards.js";

export class ImportMemberCompletionProvider implements vscode.CompletionItemProvider {
  readonly #daemon: DaemonManager;

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

    if (!shouldOfferImportCompletions(getImportLensConfig())) {
      return undefined;
    }

    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

    if (this.#daemon.state !== "ready" && await this.#daemon.start(workspaceRoot) !== "ready") {
      return undefined;
    }

    const response = await this.#daemon.completeImportMembers({
      type: "complete_import_members",
      version: protocolVersion,
      request_id: nextIpcRequestId(),
      workspace_root: workspaceRoot,
      active_document_path: document.fileName,
      source: document.getText(),
      cursor_offset: document.offsetAt(position),
    });

    if (!response || response.error || !response.specifier || token.isCancellationRequested) {
      return undefined;
    }

    const importedNames = new Set(response.imported_names);

    return response.exports
      .filter((exportedName) => exportedName !== "default")
      .filter((exportedName) => !importedNames.has(exportedName))
      .sort((left, right) => left.localeCompare(right))
      .map((exportedName) => {
        const item = new vscode.CompletionItem(exportedName, vscode.CompletionItemKind.Variable);
        item.detail = response.specifier ?? undefined;
        return item;
      });
  }
}
