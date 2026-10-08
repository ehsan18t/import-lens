import * as vscode from "vscode";
import type { Logger } from "../logging/types.js";
import { newlyVisibleDocuments } from "../visibleDocuments.js";
import { analysisRootForFile } from "../workspaceContext.js";
import {
  type PackageJsonPrewarmTarget,
  prewarmPackageJsonDocuments,
} from "./packageJsonHelpers.js";

// The root PackageJsonAnalysisController resolves for the same document.
const packageJsonAnalysisRoot = (document: vscode.TextDocument): Promise<string> =>
  analysisRootForFile(
    document.fileName,
    vscode.workspace.getWorkspaceFolder(document.uri)?.uri.fsPath,
  );

/**
 * Fire-and-forget prewarm of the manifests among `documents`. Resolving each analysis root reads
 * the file system, so the sends happen asynchronously and a failure is only logged.
 */
export const prewarmPackageJsonManifests = (
  documents: readonly vscode.TextDocument[],
  target: PackageJsonPrewarmTarget,
  logger: Pick<Logger, "debug" | "warn">,
  describeSent: (count: number) => string,
): void => {
  prewarmPackageJsonDocuments(documents, target, packageJsonAnalysisRoot).then(
    (sent) => {
      if (sent > 0) {
        logger.debug(describeSent(sent));
      }
    },
    (error: unknown) => {
      logger.warn(
        `package.json prewarm failed: ${error instanceof Error ? error.message : String(error)}`,
      );
    },
  );
};

export const registerPackageJsonPrewarm = (
  context: vscode.ExtensionContext,
  target: PackageJsonPrewarmTarget,
  logger: Pick<Logger, "debug" | "warn">,
): void => {
  const sendPrewarm = (documents: readonly vscode.TextDocument[]): void => {
    prewarmPackageJsonManifests(
      documents,
      target,
      logger,
      (sent) => `Sent package.json prewarm for ${sent} manifest(s).`,
    );
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
