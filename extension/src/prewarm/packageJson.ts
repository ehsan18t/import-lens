import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import type { Logger } from "../logging/types.js";
import { prewarmPackageJsonDocuments } from "./packageJsonHelpers.js";

export const registerPackageJsonPrewarm = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "debug">,
): void => {
  const sendPrewarm = (document: vscode.TextDocument): void => {
    const sent = prewarmPackageJsonDocuments([document], daemon);

    if (sent > 0) {
      logger.debug(`Sent package.json prewarm for ${document.uri.fsPath}.`);
    }
  };

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(sendPrewarm),
    vscode.workspace.onDidSaveTextDocument(sendPrewarm),
  );

  prewarmPackageJsonDocuments(vscode.workspace.textDocuments, daemon);
};
