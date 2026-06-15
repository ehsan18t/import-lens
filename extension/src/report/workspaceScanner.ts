import path from "node:path";
import { createImportRequest } from "../analysis/request.js";
import { loadImportLensIgnore, shouldIgnoreImport } from "../imports/ignore.js";
import { extractRuntimeImports } from "../imports/parser.js";
import { getPackageName } from "../imports/specifier.js";
import { resolveInstalledPackagesByName } from "../imports/resolver.js";
import type { DetectedImport } from "../imports/types.js";
import type { BatchRequest, BatchResponse, ImportRequest } from "../ipc/protocol.js";
import { protocolVersion } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import { mapWithConcurrency } from "./concurrency.js";
import type { WorkspaceReportItem } from "./reportModel.js";

export const workspaceIncludePattern = "**/*.{js,jsx,ts,tsx,mts,cts,svelte,astro,vue}";
export const workspaceExcludePattern = "**/{node_modules,dist,build,out,coverage}/**";
const DEFAULT_BATCH_SIZE = 50;
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
  sendBatch(request: BatchRequest): Promise<BatchResponse | null>;
}

export interface ScannedImport {
  detected: DetectedImport;
  sourceFile: string;
  workspaceRoot: string;
  request?: ImportRequest;
  warning?: string;
}

interface WorkspaceScannerOptions {
  chunkSize?: number;
  nextRequestId?: () => number;
  scanConcurrency?: number;
}

export const buildWorkspaceReportItems = async (
  workspace: WorkspaceScannerApi,
  daemon: WorkspaceReportDaemon,
  options: WorkspaceScannerOptions = {},
): Promise<WorkspaceReportItem[]> => {
  const scannedImports = await scanWorkspaceImports(workspace, options);
  return analyzeScannedImports(scannedImports, daemon, options);
};

export const scanWorkspaceImports = async (
  workspace: WorkspaceScannerApi,
  options: WorkspaceScannerOptions = {},
): Promise<ScannedImport[]> => {
  const uris = sortWorkspaceUris(await workspace.findFiles(workspaceIncludePattern, workspaceExcludePattern));
  const scannedImportGroups = await mapWithConcurrency(
    uris,
    options.scanConcurrency ?? DEFAULT_SCAN_CONCURRENCY,
    async (uri) => scanWorkspaceUri(workspace, uri),
  );

  return scannedImportGroups.flat();
};

const scanWorkspaceUri = async (
  workspace: WorkspaceScannerApi,
  uri: WorkspaceUri,
): Promise<ScannedImport[]> => {
  const scannedImports: ScannedImport[] = [];
  let document: WorkspaceTextDocument;
  let workspaceRoot: string;
  let detectedImports: DetectedImport[];

  try {
    document = await workspace.openTextDocument(uri);
    workspaceRoot = workspace.getWorkspaceFolder?.(document.uri)?.uri.fsPath ?? path.dirname(document.fileName);
    const ignoreRules = await loadImportLensIgnore(document.fileName);
    detectedImports = extractRuntimeImports(document.fileName, document.getText())
      .filter((detected) => !shouldIgnoreImport(detected, document.fileName, ignoreRules));
  } catch {
    return [];
  }

  const packageResolutions = await resolveInstalledPackagesByName(
    detectedImports.map((detected) => detected.specifier),
    document.fileName,
  );

  for (const detected of detectedImports) {
    try {
      const resolution = packageResolutions.get(getPackageName(detected.specifier))
        ?? { ok: false as const, packageName: getPackageName(detected.specifier), reason: "package_not_found" as const };

      if (!resolution.ok) {
        scannedImports.push({
          detected,
          sourceFile: document.fileName,
          workspaceRoot,
          warning: resolution.reason === "package_not_found" ? "Package not found" : "Invalid package.json",
        });
        continue;
      }

      scannedImports.push({
        detected,
        sourceFile: document.fileName,
        workspaceRoot,
        request: createImportRequest(detected, resolution.version),
      });
    } catch {
      continue;
    }
  }

  return scannedImports;
};

export const analyzeScannedImports = async (
  scannedImports: readonly ScannedImport[],
  daemon: WorkspaceReportDaemon,
  options: WorkspaceScannerOptions = {},
): Promise<WorkspaceReportItem[]> => {
  const chunkSize = options.chunkSize ?? DEFAULT_BATCH_SIZE;
  const nextRequestId = options.nextRequestId ?? nextIpcRequestId;
  const reportItems: WorkspaceReportItem[] = scannedImports
    .filter((item) => !item.request)
    .map((item) => ({
      detected: item.detected,
      sourceFile: item.sourceFile,
      workspaceRoot: item.workspaceRoot,
      warning: item.warning ?? "Skipped",
    }));

  const requestableImports = scannedImports.filter(hasRequest);

  if (daemon.state !== "ready") {
    return [
      ...reportItems,
      ...requestableImports.map((item) => ({
        detected: item.detected,
        sourceFile: item.sourceFile,
        workspaceRoot: item.workspaceRoot,
        warning: "Daemon unavailable",
      })),
    ];
  }

  for (const group of groupBySourceFile(requestableImports).values()) {
    for (const chunk of chunkArray(group, chunkSize)) {
      const response = await daemon.sendBatch({
        version: protocolVersion,
        request_id: nextRequestId(),
        workspace_root: chunk[0]?.workspaceRoot ?? "",
        active_document_path: chunk[0]?.sourceFile ?? "",
        imports: chunk.map((item) => item.request),
      });

      for (const [index, item] of chunk.entries()) {
        const result = response?.imports[index];

        reportItems.push({
          detected: item.detected,
          sourceFile: item.sourceFile,
          workspaceRoot: item.workspaceRoot,
          result,
          warning: result ? undefined : "No daemon response",
        });
      }
    }
  }

  return reportItems;
};

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

const hasRequest = (item: ScannedImport): item is ScannedImport & { request: ImportRequest } =>
  item.request !== undefined;

const groupBySourceFile = (
  items: readonly (ScannedImport & { request: ImportRequest })[],
): Map<string, (ScannedImport & { request: ImportRequest })[]> => {
  const groups = new Map<string, (ScannedImport & { request: ImportRequest })[]>();

  for (const item of items) {
    const key = `${item.workspaceRoot}\0${item.sourceFile}`;
    const group = groups.get(key) ?? [];
    group.push(item);
    groups.set(key, group);
  }

  return groups;
};
