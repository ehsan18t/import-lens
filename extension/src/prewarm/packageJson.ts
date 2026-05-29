import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { packageJsonPrewarmPayload } from "./packageJsonHelpers.js";

export const registerPackageJsonPrewarm = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
): void => {
  const sendPrewarm = (document: vscode.TextDocument): void => {
    if (document.uri.scheme !== "file") {
      return;
    }

    const payload = packageJsonPrewarmPayload(document.uri.fsPath);

    if (!payload) {
      return;
    }

    daemon.prewarmPackageJson(payload.packageJsonPath, payload.activeDocumentPath);
  };

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(sendPrewarm),
    vscode.workspace.onDidSaveTextDocument(sendPrewarm),
  );

  for (const document of vscode.workspace.textDocuments) {
    sendPrewarm(document);
  }
};
