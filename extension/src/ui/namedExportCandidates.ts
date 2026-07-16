import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import type { DaemonManager } from "../daemon/manager.js";
import { type DetectedImport, protocolVersion } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { ImportLensLogger } from "../logger.js";
import { analysisRootForFile } from "../workspaceContext.js";

export const showNamedExportCandidatesCommand = "importLens.showNamedExportCandidates";

export const showNamedExportCandidates = async (
  daemon: DaemonManager,
  logger: ImportLensLogger,
  uri: vscode.Uri,
  detected: DetectedImport,
): Promise<void> => {
  if (!getImportLensConfig().enabled) {
    return;
  }

  const workspaceFolder = vscode.workspace.getWorkspaceFolder(uri);
  const workspaceRoot = await analysisRootForFile(uri.fsPath, workspaceFolder?.uri.fsPath);

  if (daemon.state !== "ready" && (await daemon.start(workspaceRoot)) !== "ready") {
    await vscode.window.showWarningMessage("Import Lens daemon is unavailable.");
    return;
  }

  const analysis = await daemon.analyzeSpecifiers({
    type: "analyze_specifiers",
    version: protocolVersion,
    request_id: nextIpcRequestId(),
    workspace_root: workspaceRoot,
    active_document_path: uri.fsPath,
    specifiers: [detected.specifier],
  });
  const request = analysis?.imports.find(
    (item) => item.detected.specifier === detected.specifier,
  )?.request;

  if (!request) {
    await vscode.window.showWarningMessage(`Import Lens could not resolve ${detected.specifier}.`);
    return;
  }

  // The daemon owns runtime classification (ADR-0002); we only tell it WHERE the import is.
  // The offset of the import statement lands in whichever script region the daemon then
  // classifies — Server for Astro frontmatter, Component for a plain file — so the exports we
  // enumerate resolve under the same conditions their size does.
  const document = await vscode.workspace.openTextDocument(uri);
  const cursorOffset = document.offsetAt(
    new vscode.Position(
      detected.specifierRange.start.line,
      detected.specifierRange.start.character,
    ),
  );

  const response = await daemon.enumerateExports({
    type: "enumerate_exports",
    version: protocolVersion,
    request_id: nextIpcRequestId(),
    workspace_root: workspaceRoot,
    active_document_path: uri.fsPath,
    specifier: detected.specifier,
    package: request.package,
    package_version: request.version,
    cursor_offset: cursorOffset,
  });

  if (!response || response.error) {
    logger.warn(
      `Named export candidates unavailable for ${detected.specifier}: ${response?.error ?? "no daemon response"}`,
    );
    await vscode.window.showWarningMessage(
      `Import Lens could not enumerate exports for ${detected.specifier}.`,
    );
    return;
  }

  const exports = response.exports
    .filter((exportedName) => exportedName !== "default")
    .sort((left, right) => left.localeCompare(right));

  if (exports.length === 0) {
    await vscode.window.showInformationMessage(
      `Import Lens found no named exports for ${detected.specifier}.`,
    );
    return;
  }

  const selected = await vscode.window.showQuickPick(
    exports.map((exportedName) => ({ label: exportedName })),
    {
      canPickMany: true,
      title: `Named exports from ${detected.specifier}`,
      placeHolder: "Select exports to copy as a named import",
    },
  );

  if (!selected || selected.length === 0) {
    return;
  }

  const names = selected.map((item) => item.label).sort((left, right) => left.localeCompare(right));
  await vscode.env.clipboard.writeText(
    `import { ${names.join(", ")} } from '${detected.specifier}';`,
  );
  await vscode.window.showInformationMessage(`Copied named import for ${detected.specifier}.`);
};
