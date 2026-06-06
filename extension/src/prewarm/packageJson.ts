import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { prewarmPackageJsonDocuments } from "./packageJsonHelpers.js";

export const registerPackageJsonPrewarm = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
): void => {
  const sendPrewarm = (document: vscode.TextDocument): void => {
    prewarmPackageJsonDocuments([document], daemon);
  };

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(sendPrewarm),
    vscode.workspace.onDidSaveTextDocument(sendPrewarm),
  );

  prewarmPackageJsonDocuments(vscode.workspace.textDocuments, daemon);
};
