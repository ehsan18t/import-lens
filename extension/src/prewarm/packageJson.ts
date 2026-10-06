import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import type { Logger } from "../logging/types.js";
import { newlyVisibleDocuments } from "../visibleDocuments.js";
import { prewarmPackageJsonDocuments } from "./packageJsonHelpers.js";

export const registerPackageJsonPrewarm = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "debug">,
): void => {
  const sendPrewarm = (documents: readonly vscode.TextDocument[]): void => {
    const sent = prewarmPackageJsonDocuments(documents, daemon);

    if (sent > 0) {
      logger.debug(`Sent package.json prewarm for ${sent} manifest(s).`);
    }
  };

  // A manifest is prewarmed when it becomes visible or is saved, never merely opened: VS Code and
  // other extensions open manifests nobody looks at, and each prewarm builds every dependency.
  let visibleKeys: ReadonlySet<string> = new Set();
  const syncVisible = (editors: readonly vscode.TextEditor[]): void => {
    const documents = editors.map((editor) => editor.document);
    sendPrewarm(newlyVisibleDocuments(documents, visibleKeys));
    visibleKeys = new Set(documents.map((document) => document.uri.toString()));
  };

  context.subscriptions.push(
    vscode.window.onDidChangeVisibleTextEditors(syncVisible),
    vscode.workspace.onDidSaveTextDocument((document) => sendPrewarm([document])),
  );

  syncVisible(vscode.window.visibleTextEditors);
};
