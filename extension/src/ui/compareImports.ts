import * as vscode from "vscode";
import { createImportRequest } from "../analysis/request.js";
import type { DaemonManager } from "../daemon/manager.js";
import { getPackageName } from "../imports/specifier.js";
import { resolveInstalledPackage } from "../imports/resolver.js";
import type { DetectedImport } from "../imports/types.js";
import { protocolVersion, type ImportRequest } from "../ipc/protocol.js";
import { analysisRootForFile } from "../workspaceContext.js";
import { formatBytes } from "./format.js";

let compareRequestId = Date.now();

export const compareImportsCommand = "importLens.compareImports";

export const compareImports = async (
  daemon: DaemonManager,
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

  const imports: ImportRequest[] = [];

  for (const specifier of specifiers) {
    const resolution = await resolveInstalledPackage(specifier, editor.document.fileName);
    if (!resolution.ok) {
      continue;
    }

    imports.push(createImportRequest(detectedImport(specifier), resolution.version));
  }

  if (imports.length === 0) {
    await vscode.window.showWarningMessage("ImportLens could not resolve any imports to compare.");
    return;
  }

  const workspaceFolder = vscode.workspace.getWorkspaceFolder(editor.document.uri);
  const workspaceRoot = await analysisRootForFile(editor.document.fileName, workspaceFolder?.uri.fsPath);

  if (daemon.state !== "ready" && await daemon.start(workspaceRoot) !== "ready") {
    await vscode.window.showWarningMessage("ImportLens daemon is unavailable.");
    return;
  }

  const response = await daemon.sendBatch({
    version: protocolVersion,
    request_id: ++compareRequestId,
    workspace_root: workspaceRoot,
    active_document_path: editor.document.fileName,
    imports,
  });

  const items = (response?.imports ?? [])
    .filter((result) => !result.error)
    .sort((left, right) => left.brotli_bytes - right.brotli_bytes)
    .map((result) => ({
      label: `${result.specifier}: ${formatBytes(result.brotli_bytes)} br`,
      detail: `${formatBytes(result.minified_bytes)} min · ${formatBytes(result.gzip_bytes)} gz · ${formatBytes(result.zstd_bytes)} zstd`,
    }));

  await vscode.window.showQuickPick(items, {
    title: "ImportLens Import Comparison",
    placeHolder: "Imports sorted by Brotli size",
  });
};

const detectedImport = (specifier: string): DetectedImport => ({
  specifier,
  packageName: getPackageName(specifier),
  named: [],
  importKind: "namespace",
  syntax: "static",
  runtime: "component",
  line: 0,
  quoteEnd: { line: 0, character: 0 },
  statementRange: {
    start: { line: 0, character: 0 },
    end: { line: 0, character: 0 },
  },
});
