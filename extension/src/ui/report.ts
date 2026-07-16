import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import { protocolVersion } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { Logger } from "../logging/types.js";
import { workspaceReportHtml } from "./reportContent.js";

export const showReport = async (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
): Promise<void> => {
  logger.info("Building workspace report.");
  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

  if (!workspaceRoot) {
    await vscode.window.showWarningMessage("Import Lens report requires an open workspace folder.");
    return;
  }

  if (daemon.state !== "ready" && (await daemon.start(workspaceRoot)) !== "ready") {
    await vscode.window.showWarningMessage("Import Lens daemon is unavailable.");
    return;
  }

  try {
    const config = getImportLensConfig();
    const response = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Import Lens: Building workspace report",
      },
      () =>
        daemon.requestWorkspaceReport({
          type: "workspace_report",
          version: protocolVersion,
          request_id: nextIpcRequestId(),
          workspace_root: workspaceRoot,
          // The per-import budget only. The report's rows are imports; the per-file budget is
          // judged against a File Cost, which lives in the editor's diagnostics and in
          // `importlens check` (ADR-0004, SRS FR-036i).
          budgets: { perImportBrotliBytes: config.budgets.perImportBrotliBytes },
        }),
    );

    if (!response || response.error) {
      await vscode.window.showWarningMessage(
        `Import Lens report unavailable${response?.error ? `: ${response.error}` : "."}`,
      );
      return;
    }

    logger.info(`Workspace report built with ${response.rows.length} import item(s).`);
    const panel = vscode.window.createWebviewPanel(
      "importLensReport",
      "Import Lens Report",
      vscode.ViewColumn.Beside,
      {
        enableScripts: false,
      },
    );

    // Every claim the panel makes — the headline metric's name included — is rendered by
    // `reportContent`, which is vscode-free precisely so a test can read what the user is told.
    panel.webview.html = workspaceReportHtml({ rows: response.rows, summary: response.summary });
    context.subscriptions.push(panel);
  } catch (error) {
    logger.warn(
      `Workspace report request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
    await vscode.window.showWarningMessage(
      `Import Lens report request failed: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
};
