import { protocolVersion, type AnalyzeDocumentRequest, type AnalyzeDocumentResponse, type DetectedImport, type ImportResult } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import { mapWithConcurrency } from "./concurrency.js";
import type { WorkspaceReportItem } from "./reportModel.js";

export const workspaceIncludePattern = "**/*.{js,jsx,ts,tsx,mts,cts,svelte,astro,vue}";
export const workspaceExcludePattern = "**/{node_modules,dist,build,out,coverage}/**";
const DEFAULT_SCAN_CONCURRENCY = 8;

export interface WorkspaceUri {
  fsPath: string;
}

export interface WorkspaceTextDocument {
  uri: WorkspaceUri;
  fileName: string;
  getText(): string;
}

export interface WorkspaceFolder {
  uri: WorkspaceUri;
}

export interface WorkspaceScannerApi {
  findFiles(include: string, exclude: string): Promise<readonly WorkspaceUri[]>;
  openTextDocument(uri: WorkspaceUri): Promise<WorkspaceTextDocument>;
  getWorkspaceFolder?(uri: WorkspaceUri): WorkspaceFolder | undefined;
}

export interface WorkspaceReportDaemon {
  readonly state: "ready" | "unavailable";
  analyzeDocument(request: AnalyzeDocumentRequest): Promise<AnalyzeDocumentResponse | null>;
}

export interface ScannedImport {
  detected: DetectedImport;
  sourceFile: string;
  workspaceRoot: string;
  result?: ImportResult;
  warning?: string;
}

interface WorkspaceScannerOptions {
  nextRequestId?: () => number;
  scanConcurrency?: number;
}

export const buildWorkspaceReportItems = async (
  workspace: WorkspaceScannerApi,
  daemon: WorkspaceReportDaemon,
  options: WorkspaceScannerOptions = {},
): Promise<WorkspaceReportItem[]> => {
  const scannedImports = await scanWorkspaceImports(workspace, daemon, options);
  return analyzeScannedImports(scannedImports);
};

export const scanWorkspaceImports = async (
  workspace: WorkspaceScannerApi,
  daemon: WorkspaceReportDaemon,
  options: WorkspaceScannerOptions = {},
): Promise<ScannedImport[]> => {
  if (daemon.state !== "ready") {
    return [];
  }

  const uris = sortWorkspaceUris(await workspace.findFiles(workspaceIncludePattern, workspaceExcludePattern));
  const scannedImportGroups = await mapWithConcurrency(
    uris,
    options.scanConcurrency ?? DEFAULT_SCAN_CONCURRENCY,
    async (uri) => scanWorkspaceUri(workspace, daemon, uri, options.nextRequestId ?? nextIpcRequestId),
  );

  return scannedImportGroups.flat();
};

const scanWorkspaceUri = async (
  workspace: WorkspaceScannerApi,
  daemon: WorkspaceReportDaemon,
  uri: WorkspaceUri,
  nextRequestId: () => number,
): Promise<ScannedImport[]> => {
  let document: WorkspaceTextDocument;
  let workspaceRoot: string;

  try {
    document = await workspace.openTextDocument(uri);
    workspaceRoot = workspace.getWorkspaceFolder?.(document.uri)?.uri.fsPath ?? document.fileName;
  } catch {
    return [];
  }

  const response = await daemon.analyzeDocument({
    type: "analyze_document",
    version: protocolVersion,
    request_id: nextRequestId(),
    workspace_root: workspaceRoot,
    active_document_path: document.fileName,
    source: document.getText(),
  });

  if (!response || response.error) {
    return [];
  }

  return response.imports.map((item) => ({
    detected: item.detected,
    sourceFile: document.fileName,
    workspaceRoot,
    result: item.result,
    warning: item.result ? undefined : item.message ?? "Skipped",
  }));
};

export const analyzeScannedImports = async (
  scannedImports: readonly ScannedImport[],
): Promise<WorkspaceReportItem[]> =>
  scannedImports.map((item) => ({
    detected: item.detected,
    sourceFile: item.sourceFile,
    workspaceRoot: item.workspaceRoot,
    result: item.result,
    warning: item.warning,
  }));
export const sortWorkspaceUris = <T extends WorkspaceUri>(uris: readonly T[]): T[] =>
  [...uris].sort((left, right) => left.fsPath.localeCompare(right.fsPath));

export const chunkArray = <T>(items: readonly T[], chunkSize: number): T[][] => {
  const size = Math.max(1, chunkSize);
  const chunks: T[][] = [];

  for (let index = 0; index < items.length; index += size) {
    chunks.push(items.slice(index, index + size));
  }

  return chunks;
};
