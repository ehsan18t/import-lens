import * as vscode from "vscode";
import {
  bundleImpactHistoryKey,
  bundleImpactHistoryDeltaLabel,
  previousBundleImpactForFile,
  recordBundleImpactHistory,
  type BundleImpactHistoryItem,
} from "../analysis/history.js";
import { formatCurrentFileSizeSummary } from "../analysis/fileSize.js";
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
    await vscode.window.showWarningMessage("No active editor is available for ImportLens sizing.");
    return;
  }

  const { document } = editor;
  if (document.uri.scheme !== "file" || !supportedLanguageIds.has(document.languageId)) {
    await vscode.window.showWarningMessage(
      "ImportLens current-file sizing supports local JavaScript and TypeScript files.",
    );
    return;
  }

  const config = getImportLensConfig();
  const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
  const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

  if (daemon.state !== "ready" && (await daemon.start(workspaceRoot)) !== "ready") {
    await vscode.window.showWarningMessage("ImportLens daemon is unavailable.");
    return;
  }

  try {
    const response = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "ImportLens: Calculating current file size",
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
        "ImportLens daemon did not return a current-file size.",
      );
      return;
    }

    if (response.error) {
      await vscode.window.showWarningMessage(
        `ImportLens current-file size unavailable: ${response.error}`,
      );
      return;
    }

    if (response.states.length === 0) {
      await vscode.window.showInformationMessage("Current file: no runtime package imports found.");
      return;
    }

    if (response.imports.length === 0) {
      await vscode.window.showWarningMessage(
        "ImportLens could not resolve any runtime package imports in the current file.",
      );
      return;
    }

    const currentHistoryItem: BundleImpactHistoryItem = {
      timestamp: Date.now(),
      fileName: document.fileName,
      rawBytes: response.raw_bytes,
      minifiedBytes: response.minified_bytes,
      gzipBytes: response.gzip_bytes,
      brotliBytes: response.brotli_bytes,
      zstdBytes: response.zstd_bytes,
      importCount: response.imports.length,
    };
    const previous = previousBundleImpactForFile(
      context.globalState.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []),
      document.fileName,
    );
    await recordBundleImpactHistory(context.globalState, currentHistoryItem);

    const skipped = response.states.length - response.imports.length;
    const skippedSuffix = skipped > 0 ? ` · ${skipped} skipped` : "";
    const diffSuffix = previous
      ? ` · ${bundleImpactHistoryDeltaLabel(currentHistoryItem, previous)}`
      : "";
    await vscode.window.showInformationMessage(
      `${formatCurrentFileSizeSummary(response, config.compression)}${skippedSuffix}${diffSuffix}`,
    );
  } catch (error) {
    logger.warn(
      `Current-file size request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    await vscode.window.showWarningMessage("ImportLens current-file size request failed.");
  }
};

export const showBundleImpactHistory = async (context: vscode.ExtensionContext): Promise<void> => {
  const history = context.globalState.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []);

  if (history.length === 0) {
    await vscode.window.showInformationMessage("ImportLens bundle impact history is empty.");
    return;
  }

  const panel = vscode.window.createWebviewPanel(
    "importLensBundleImpactHistory",
    "ImportLens Bundle Impact History",
    vscode.ViewColumn.Beside,
    {
      enableScripts: false,
      retainContextWhenHidden: false,
    },
  );
  panel.webview.html = bundleImpactHistoryHtml(history);
};
