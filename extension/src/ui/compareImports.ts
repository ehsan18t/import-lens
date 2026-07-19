import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { protocolVersion } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { Logger } from "../logging/types.js";
import { analysisRootForFile } from "../workspaceContext.js";
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
    title: "Import Lens: Compare Imports",
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
    await vscode.window.showWarningMessage("Import Lens daemon is unavailable.");
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
    await vscode.window.showWarningMessage("Import Lens import comparison failed.");
    return;
  }

  // The whole response, not just the results: an item with no result still names the specifier the
  // user asked about and carries the daemon's reason for it, and dropping it here is what made a
  // four-package comparison silently render two rows.
  const { items, comparedCount, excludedCount, warning } = compareImportItemsForResults(
    specifiers,
    response?.imports ?? null,
  );

  if (warning) {
    await vscode.window.showWarningMessage(warning);
    return;
  }

  const pickItems: vscode.QuickPickItem[] = items.map((item) =>
    item.separator
      ? { label: item.label, kind: vscode.QuickPickItemKind.Separator }
      : { label: item.label, detail: item.detail },
  );

  await vscode.window.showQuickPick(pickItems, {
    title:
      excludedCount > 0
        ? `Import Lens Import Comparison — ${comparedCount} of ${comparedCount + excludedCount} compared`
        : "Import Lens Import Comparison",
    placeHolder: "Imports sorted by Brotli size",
    matchOnDetail: true,
  });
};
