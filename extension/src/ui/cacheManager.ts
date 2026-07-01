import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { CacheCleanupResponse, CacheRemoveResponse } from "../ipc/protocol.js";
import type { Logger } from "../logging/types.js";
import { analysisRootForFile } from "../workspaceContext.js";
import {
  cacheManagerActionItems,
  cacheShardPickItems,
} from "./cacheManagerItems.js";
import {
  cacheCleanupRequest,
  cacheListRequest,
  cacheRemoveAllRequest,
  cacheRemoveCurrentProjectRequest,
  cacheRemoveSelectedRequest,
  cacheStatusRequest,
} from "./cacheManagerRequests.js";

export const manageCacheCommand = "importLens.manageCache";
export const clearCurrentProjectCacheCommand = "importLens.clearCache";
export const clearAllCachesCommand = "importLens.clearAllCaches";

export const showCacheManager = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const status = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Loading cache status",
    },
    () => daemon.cacheStatus(cacheStatusRequest(nextIpcRequestId(), workspaceRoot)),
  );

  if (!status) {
    await vscode.window.showWarningMessage("ImportLens cache status is unavailable.");
    return;
  }

  if (status.error) {
    logger.warn(`Cache status failed: ${status.error}`);
    await vscode.window.showWarningMessage(`ImportLens cache status failed: ${status.error}`);
    return;
  }

  const selected = await vscode.window.showQuickPick(cacheManagerActionItems(status), {
    title: "ImportLens Cache",
    placeHolder: "Choose a cache maintenance action",
  });

  if (!selected || selected.action === "summary") {
    return;
  }

  if (selected.action === "cleanup") {
    await cleanupCache(daemon, logger, afterMutation);
    return;
  }

  if (selected.action === "clearCurrent") {
    await clearCurrentProjectCache(daemon, logger, afterMutation);
    return;
  }

  if (selected.action === "clearAll") {
    await clearAllCaches(daemon, logger, afterMutation);
    return;
  }

  await inspectProjectCaches(daemon, logger, afterMutation);
};

export const clearCurrentProjectCache = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const confirmed = await vscode.window.showWarningMessage(
    "Clear the ImportLens cache for the current project?",
    { modal: true },
    "Clear Cache",
  );

  if (confirmed !== "Clear Cache") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Clearing current project cache",
    },
    () => daemon.removeCache(cacheRemoveCurrentProjectRequest(nextIpcRequestId(), workspaceRoot)),
  );

  await reportRemoveResponse(logger, "current project", response);
  notifyAfterMutation(response, afterMutation);
};

export const clearAllCaches = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const confirmed = await vscode.window.showWarningMessage(
    "Clear all ImportLens project caches?",
    { modal: true },
    "Clear All",
  );

  if (confirmed !== "Clear All") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Clearing all project caches",
    },
    () => daemon.removeCache(cacheRemoveAllRequest(nextIpcRequestId())),
  );

  await reportRemoveResponse(logger, "all projects", response);
  notifyAfterMutation(response, afterMutation);
};

const cleanupCache = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Cleaning up project caches",
    },
    () => daemon.cleanupCache(cacheCleanupRequest(nextIpcRequestId())),
  );

  await reportCleanupResponse(logger, response);
  notifyAfterMutation(response, afterMutation);
};

const inspectProjectCaches = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Loading project caches",
    },
    () => daemon.listCache(cacheListRequest(nextIpcRequestId())),
  );

  if (!response) {
    await vscode.window.showWarningMessage("ImportLens project cache list is unavailable.");
    return;
  }

  if (response.error) {
    logger.warn(`Cache list failed: ${response.error}`);
    await vscode.window.showWarningMessage(`ImportLens cache list failed: ${response.error}`);
    return;
  }

  const items = cacheShardPickItems(response);

  if (items.length === 0) {
    await vscode.window.showInformationMessage("ImportLens has no project caches yet.");
    return;
  }

  const selected = await vscode.window.showQuickPick(items, {
    canPickMany: true,
    title: "ImportLens Project Caches",
    placeHolder: "Select project caches to remove",
  });

  if (!selected || selected.length === 0) {
    return;
  }

  const confirmed = await vscode.window.showWarningMessage(
    `Remove ${selected.length} selected ImportLens project cache${selected.length === 1 ? "" : "s"}?`,
    { modal: true },
    "Remove Selected",
  );

  if (confirmed !== "Remove Selected") {
    return;
  }

  const removeResponse = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "ImportLens: Removing selected project caches",
    },
    () => daemon.removeCache(cacheRemoveSelectedRequest(
      nextIpcRequestId(),
      selected.map((item) => item.shardId),
    )),
  );

  await reportRemoveResponse(logger, "selected projects", removeResponse);
  notifyAfterMutation(removeResponse, afterMutation);
};

const requireWorkspaceRoot = async (): Promise<string | null> => {
  const workspaceRoot = await currentWorkspaceRoot();

  if (!workspaceRoot) {
    await vscode.window.showWarningMessage("Open a workspace or local file before managing ImportLens cache.");
  }

  return workspaceRoot;
};

const currentWorkspaceRoot = async (): Promise<string | null> => {
  const editor = vscode.window.activeTextEditor;

  if (editor?.document.uri.scheme === "file") {
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(editor.document.uri);
    return analysisRootForFile(editor.document.fileName, workspaceFolder?.uri.fsPath);
  }

  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
};

const ensureDaemonReady = async (daemon: DaemonManager, workspaceRoot: string): Promise<boolean> => {
  if (daemon.state === "ready" || await daemon.start(workspaceRoot) === "ready") {
    return true;
  }

  await vscode.window.showWarningMessage("ImportLens daemon is unavailable.");
  return false;
};

const reportCleanupResponse = async (
  logger: Pick<Logger, "info" | "warn">,
  response: CacheCleanupResponse | null,
): Promise<void> => {
  if (!response) {
    await vscode.window.showWarningMessage("ImportLens cache cleanup did not return a result.");
    return;
  }

  if (response.error) {
    logger.warn(`Cache cleanup failed: ${response.error}`);
    await vscode.window.showWarningMessage(`ImportLens cache cleanup failed: ${response.error}`);
    return;
  }

  if (response.failed.length > 0) {
    logger.warn(`Cache cleanup removed ${response.removed.length} cache(s), failed ${response.failed.length}.`);
    await vscode.window.showWarningMessage(
      `ImportLens cache cleanup removed ${response.removed.length} cache(s), failed ${response.failed.length}.`,
    );
    return;
  }

  logger.info(`Cache cleanup removed ${response.removed.length} project cache(s).`);
  await vscode.window.showInformationMessage(
    `ImportLens cache cleanup removed ${response.removed.length} project cache(s).`,
  );
};

const reportRemoveResponse = async (
  logger: Pick<Logger, "info" | "warn">,
  scopeLabel: string,
  response: CacheRemoveResponse | null,
): Promise<void> => {
  if (!response) {
    await vscode.window.showWarningMessage("ImportLens cache removal did not return a result.");
    return;
  }

  if (response.error) {
    logger.warn(`Cache removal failed for ${scopeLabel}: ${response.error}`);
    await vscode.window.showWarningMessage(`ImportLens cache removal failed: ${response.error}`);
    return;
  }

  if (response.failed.length > 0) {
    logger.warn(`Cache removal for ${scopeLabel} removed ${response.removed.length} cache(s), failed ${response.failed.length}.`);
    await vscode.window.showWarningMessage(
      `ImportLens removed ${response.removed.length} cache(s), failed ${response.failed.length}.`,
    );
    return;
  }

  logger.info(`Removed ${response.removed.length} ImportLens cache(s) for ${scopeLabel}.`);
  await vscode.window.showInformationMessage(
    `Removed ${response.removed.length} ImportLens cache(s) for ${scopeLabel}.`,
  );
};

const notifyAfterMutation = (
  response: CacheCleanupResponse | CacheRemoveResponse | null,
  afterMutation?: () => void,
): void => {
  if (response && !response.error && response.removed.length > 0) {
    afterMutation?.();
  }
};
