import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { protocolVersion } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import { analysisRootForFile } from "../workspaceContext.js";
import type { Logger } from "../logging/types.js";
import { compareImportItemsForResults } from "./compareImportItems.js";

export const compareImportsCommand = "importLens.compareImports";

export const compareImports = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "debug" | "warn">,
  initialSpecifier?: string,
): Promise<void> => {
  const editor = vscode.window.activeTextEditor;

  if (!editor) {
    await vscode.window.showWarningMessage("Open a source file before comparing imports.");
    return;
  }

  const input = await vscode.window.showInputBox({
    title: "ImportLens: Compare Imports",
    prompt: "Enter package imports separated by commas",
    value: initialSpecifier ?? "",
  });

  const specifiers = (input ?? "")
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);

  if (specifiers.length === 0) {
    return;
  }

  const workspaceFolder = vscode.workspace.getWorkspaceFolder(editor.document.uri);
  const workspaceRoot = await analysisRootForFile(
    editor.document.fileName,
    workspaceFolder?.uri.fsPath,
  );

  if (daemon.state !== "ready" && (await daemon.start(workspaceRoot)) !== "ready") {
    await vscode.window.showWarningMessage("ImportLens daemon is unavailable.");
    return;
  }

  logger.debug(`Comparing ${specifiers.length} import(s): ${specifiers.join(", ")}.`);

  let response: Awaited<ReturnType<DaemonManager["analyzeSpecifiers"]>>;

  try {
    response = await daemon.analyzeSpecifiers({
      type: "analyze_specifiers",
      version: protocolVersion,
      request_id: nextIpcRequestId(),
      workspace_root: workspaceRoot,
      active_document_path: editor.document.fileName,
      specifiers,
    });
  } catch (error) {
    logger.warn(
      `Import comparison failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    await vscode.window.showWarningMessage("ImportLens import comparison failed.");
    return;
  }

  const { items, warning } = compareImportItemsForResults(
    response?.imports.flatMap((item) => (item.result ? [item.result] : [])) ?? null,
  );

  if (warning) {
    await vscode.window.showWarningMessage(warning);
    return;
  }

  await vscode.window.showQuickPick(items, {
    title: "ImportLens Import Comparison",
    placeHolder: "Imports sorted by Brotli size",
  });
};
