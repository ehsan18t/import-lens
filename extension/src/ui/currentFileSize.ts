import * as vscode from "vscode";
import {
  bundleImpactHistoryKey,
  bundleImpactHistoryLabel,
  recordBundleImpactHistory,
  type BundleImpactHistoryItem,
} from "../analysis/history.js";
import { createImportRequest } from "../analysis/request.js";
import { formatCurrentFileSizeSummary } from "../analysis/fileSize.js";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import { extractRuntimeImports } from "../imports/parser.js";
import { resolveInstalledPackage } from "../imports/resolver.js";
import { protocolVersion, type ImportRequest } from "../ipc/protocol.js";
import { supportedLanguageIds } from "../languages.js";
import type { ImportLensLogger } from "../logger.js";
import { analysisRootForFile } from "../workspaceContext.js";

let fileSizeRequestId = 0;

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
    await vscode.window.showWarningMessage("ImportLens current-file sizing supports local JavaScript and TypeScript files.");
    return;
  }

  const config = getImportLensConfig();
  const detectedImports = extractRuntimeImports(document.fileName, document.getText());

  if (detectedImports.length === 0) {
    await vscode.window.showInformationMessage("Current file: no runtime package imports found.");
    return;
  }

  const imports: ImportRequest[] = [];

  for (const detected of detectedImports) {
    const resolution = await resolveInstalledPackage(detected.specifier, document.fileName);

    if (resolution.ok) {
      imports.push(createImportRequest(detected, resolution.version));
    }
  }

  if (imports.length === 0) {
    await vscode.window.showWarningMessage("ImportLens could not resolve any runtime package imports in the current file.");
    return;
  }

  const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
  const workspaceRoot = await analysisRootForFile(document.fileName, workspaceFolder?.uri.fsPath);

  if (daemon.state !== "ready" && await daemon.start(workspaceRoot) !== "ready") {
    await vscode.window.showWarningMessage("ImportLens daemon is unavailable.");
    return;
  }

  try {
    const response = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "ImportLens: Calculating current file size",
      },
      () => daemon.requestFileSize({
        type: "file_size",
        version: protocolVersion,
        request_id: ++fileSizeRequestId,
        workspace_root: workspaceRoot,
        active_document_path: document.fileName,
        imports,
      }),
    );

    if (!response) {
      await vscode.window.showWarningMessage("ImportLens daemon did not return a current-file size.");
      return;
    }

    if (response.error) {
      await vscode.window.showWarningMessage(`ImportLens current-file size unavailable: ${response.error}`);
      return;
    }

    await recordBundleImpactHistory(context.globalState, {
      timestamp: Date.now(),
      fileName: document.fileName,
      rawBytes: response.raw_bytes,
      minifiedBytes: response.minified_bytes,
      gzipBytes: response.gzip_bytes,
      brotliBytes: response.brotli_bytes,
      zstdBytes: response.zstd_bytes,
      importCount: response.imports.length,
    });

    const skipped = detectedImports.length - imports.length;
    const skippedSuffix = skipped > 0 ? ` · ${skipped} skipped` : "";
    await vscode.window.showInformationMessage(`${formatCurrentFileSizeSummary(response, config.compression)}${skippedSuffix}`);
  } catch (error) {
    logger.warn(`Current-file size request failed: ${error instanceof Error ? error.message : String(error)}`);
    await vscode.window.showWarningMessage("ImportLens current-file size request failed.");
  }
};

export const showBundleImpactHistory = async (
  context: vscode.ExtensionContext,
): Promise<void> => {
  const history = context.globalState.get<BundleImpactHistoryItem[]>(bundleImpactHistoryKey, []);

  if (history.length === 0) {
    await vscode.window.showInformationMessage("ImportLens bundle impact history is empty.");
    return;
  }

  await vscode.window.showQuickPick(
    history.map((item) => ({
      label: bundleImpactHistoryLabel(item),
      description: new Date(item.timestamp).toLocaleString(),
      detail: item.fileName,
    })),
    {
      title: "ImportLens Bundle Impact History",
      placeHolder: "Recent current-file size measurements",
    },
  );
};
