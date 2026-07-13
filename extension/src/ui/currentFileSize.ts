import * as vscode from "vscode";
import { currentFileSizeReport } from "../analysis/fileSize.js";
import {
  type BundleImpactHistoryItem,
  bundleImpactHistoryItemForResponse,
  bundleImpactHistoryKey,
  previousBundleImpactForFile,
  recordBundleImpactHistory,
} from "../analysis/history.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import { protocolVersion } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import { supportedLanguageIds } from "../languages.js";
import type { ImportLensLogger } from "../logger.js";
import { analysisRootForFile } from "../workspaceContext.js";
import { bundleImpactHistoryHtml } from "./bundleImpactHistoryView.js";

export const showCurrentFileSize = async (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: ImportLensLogger,
): Promise<void> => {
  const editor = vscode.window.activeTextEditor;

  if (!editor) {
    await vscode.window.showWarningMessage("No active editor is available for Import Lens sizing.");
    return;
  }

  const { document } = editor;
  if (document.uri.scheme !== "file" || !supportedLanguageIds.has(document.languageId)) {
    await vscode.window.showWarningMessage(
      "Import Lens current-file sizing supports local JavaScript and TypeScript files.",
    );
    return;
  }

  const config = getImportLensConfig();
  const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
  const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

  if (daemon.state !== "ready" && (await daemon.start(workspaceRoot)) !== "ready") {
    await vscode.window.showWarningMessage("Import Lens daemon is unavailable.");
    return;
  }

  try {
    const response = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Import Lens: Calculating current file size",
      },
      () =>
        daemon.requestFileSizeDocument({
          type: "file_size_document",
          version: protocolVersion,
          request_id: nextIpcRequestId(),
          workspace_root: workspaceRoot,
          active_document_path: document.fileName,
          source: document.getText(),
        }),
    );

    if (!response) {
      await vscode.window.showWarningMessage(
        "Import Lens daemon did not return a current-file size.",
      );
      return;
    }

    if (response.error) {
      await vscode.window.showWarningMessage(
        `Import Lens current-file size unavailable: ${response.error}`,
      );
      return;
    }

    // `undefined` when the totals are a floor rather than the file: an import still being measured,
    // or one a transient engine failure sized for us. Such a number is worth SHOWING (a floor beats
    // a blank) and must never be recorded — the history has no TTL and keeps one row per file, so it
    // would become that file's baseline and make the next honest sizing read as a regression.
    const currentHistoryItem = bundleImpactHistoryItemForResponse(response, document.fileName);
    const previous = previousBundleImpactForFile(
      context.globalState.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []),
      document.fileName,
    );
    const report = currentFileSizeReport(response, config.compression, {
      current: currentHistoryItem,
      previous,
    });

    if (report.kind === "no-imports") {
      await vscode.window.showInformationMessage("Current file: no runtime package imports found.");
      return;
    }

    if (currentHistoryItem) {
      await recordBundleImpactHistory(context.globalState, currentHistoryItem);
    }

    await vscode.window.showInformationMessage(report.message);
  } catch (error) {
    logger.warn(
      `Current-file size request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    await vscode.window.showWarningMessage("Import Lens current-file size request failed.");
  }
};

export const showBundleImpactHistory = async (context: vscode.ExtensionContext): Promise<void> => {
  const history = context.globalState.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []);

  if (history.length === 0) {
    await vscode.window.showInformationMessage("Import Lens bundle impact history is empty.");
    return;
  }

  const panel = vscode.window.createWebviewPanel(
    "importLensBundleImpactHistory",
    "Import Lens Bundle Impact History",
    vscode.ViewColumn.Beside,
    {
      enableScripts: false,
      retainContextWhenHidden: false,
    },
  );
  panel.webview.html = bundleImpactHistoryHtml(history);
};
