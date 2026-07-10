import * as vscode from "vscode";
import type { DaemonManager } from "../daemon/manager.js";
import type { CacheRemoveResponse } from "../ipc/protocol.js";
import { nextIpcRequestId } from "../ipc/requestIds.js";
import type { Logger } from "../logging/types.js";
import { analysisRootForFile } from "../workspaceContext.js";
import {
  type CacheClearScope,
  cacheManagerActionItems,
  cacheRemovalToast,
} from "./cacheManagerItems.js";
import {
  cacheListRequest,
  cacheRemoveAllRequest,
  cacheRemoveCurrentProjectRequest,
  cacheRemoveOrphansRequest,
  cacheRemoveRegistryRequest,
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
      title: "Import Lens: Loading cache status",
    },
    () => daemon.cacheStatus(cacheStatusRequest(nextIpcRequestId(), workspaceRoot)),
  );

  if (!status) {
    await vscode.window.showWarningMessage("Import Lens cache status is unavailable.");
    return;
  }

  if (status.error) {
    logger.warn(`Cache status failed: ${status.error}`);
    await vscode.window.showWarningMessage(`Import Lens cache status failed: ${status.error}`);
    return;
  }

  const selected = await vscode.window.showQuickPick(cacheManagerActionItems(status), {
    title: "Import Lens Cache",
    placeHolder: "Review cache status, then choose what to clear",
  });

  // The status rows are read-only (action "summary"); picking one is a no-op.
  if (!selected || selected.action === "summary") {
    return;
  }

  switch (selected.action) {
    case "clearCurrent":
      await clearCurrentProjectCache(daemon, logger, afterMutation);
      return;
    case "clearAllProjects":
      await clearAllProjectsCache(daemon, logger, afterMutation);
      return;
    case "clearOrphans":
      await clearOrphanedCaches(daemon, logger, afterMutation);
      return;
    case "clearRegistry":
      await clearRegistryCache(daemon, logger, afterMutation);
      return;
    case "clearEverything":
      await clearAllCaches(daemon, logger, afterMutation);
      return;
  }
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
    "Clear the Import Lens cache for the current project?",
    { modal: true },
    "Clear Cache",
  );

  if (confirmed !== "Clear Cache") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Import Lens: Clearing current project cache",
    },
    () => daemon.removeCache(cacheRemoveCurrentProjectRequest(nextIpcRequestId(), workspaceRoot)),
  );

  await reportRemoveResponse(logger, "currentProject", response);
  notifyAfterRemove("currentProject", response, afterMutation);
};

// "Clear all projects": there is no dedicated daemon scope that clears every
// project shard while keeping the shared registry, so enumerate the shards and
// remove them via the Selected scope. This leaves the npm-hint registry and the
// shared resolvers intact — that is what separates it from "Clear everything".
const clearAllProjectsCache = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const list = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Import Lens: Loading project caches",
    },
    () => daemon.listCache(cacheListRequest(nextIpcRequestId())),
  );

  if (!list) {
    await vscode.window.showWarningMessage("Import Lens project cache list is unavailable.");
    return;
  }

  if (list.error) {
    logger.warn(`Cache list failed: ${list.error}`);
    await vscode.window.showWarningMessage(`Import Lens cache list failed: ${list.error}`);
    return;
  }

  const shardIds = list.shards.map((shard) => shard.shard_id);

  if (shardIds.length === 0) {
    await vscode.window.showInformationMessage("Import Lens has no project caches to clear.");
    return;
  }

  const confirmed = await vscode.window.showWarningMessage(
    `Clear Import Lens bundle caches for all ${shardIds.length} project${
      shardIds.length === 1 ? "" : "s"
    }? Registry metadata is kept.`,
    { modal: true },
    "Clear All Projects",
  );

  if (confirmed !== "Clear All Projects") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Import Lens: Clearing all project caches",
    },
    () => daemon.removeCache(cacheRemoveSelectedRequest(nextIpcRequestId(), shardIds)),
  );

  await reportRemoveResponse(logger, "allProjects", response);
  notifyAfterRemove("allProjects", response, afterMutation);
};

// "Remove orphaned caches" maps to the daemon Orphans scope: remove shards whose
// project root was moved/deleted (drive-safe — an offline drive keeps its shard)
// and scrub stale/uninstalled entries from surviving shards. Complements the
// automatic maintenance-tick sweep with an on-demand, entry-inclusive pass (RB-17).
const clearOrphanedCaches = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const confirmed = await vscode.window.showWarningMessage(
    "Remove Import Lens caches for moved or deleted projects? Caches for projects that still exist are kept.",
    { modal: true },
    "Remove Orphaned",
  );

  if (confirmed !== "Remove Orphaned") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Import Lens: Removing orphaned caches",
    },
    () => daemon.removeCache(cacheRemoveOrphansRequest(nextIpcRequestId())),
  );

  await reportRemoveResponse(logger, "orphans", response);
  notifyAfterRemove("orphans", response, afterMutation);
};

const clearRegistryCache = async (
  daemon: DaemonManager,
  logger: Pick<Logger, "info" | "warn">,
  afterMutation?: () => void,
): Promise<void> => {
  const workspaceRoot = await requireWorkspaceRoot();

  if (!workspaceRoot || !(await ensureDaemonReady(daemon, workspaceRoot))) {
    return;
  }

  const confirmed = await vscode.window.showWarningMessage(
    "Clear Import Lens registry metadata (npm hints)?",
    { modal: true },
    "Clear Registry",
  );

  if (confirmed !== "Clear Registry") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Import Lens: Clearing registry metadata",
    },
    () => daemon.removeCache(cacheRemoveRegistryRequest(nextIpcRequestId())),
  );

  await reportRemoveResponse(logger, "registry", response);
  notifyAfterRemove("registry", response, afterMutation);
};

// "Clear everything" maps to the daemon All scope: every project shard plus the
// shared registry, resolvers, and derived L1/graph caches, and it bumps the
// cache generation.
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
    "Clear everything? This removes all Import Lens project caches, registry metadata, and derived state.",
    { modal: true },
    "Clear Everything",
  );

  if (confirmed !== "Clear Everything") {
    return;
  }

  const response = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Import Lens: Clearing all caches",
    },
    () => daemon.removeCache(cacheRemoveAllRequest(nextIpcRequestId())),
  );

  await reportRemoveResponse(logger, "everything", response);
  notifyAfterRemove("everything", response, afterMutation);
};

const requireWorkspaceRoot = async (): Promise<string | null> => {
  const workspaceRoot = await currentWorkspaceRoot();

  if (!workspaceRoot) {
    await vscode.window.showWarningMessage(
      "Open a workspace or local file before managing Import Lens cache.",
    );
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

const ensureDaemonReady = async (
  daemon: DaemonManager,
  workspaceRoot: string,
): Promise<boolean> => {
  if (daemon.state === "ready" || (await daemon.start(workspaceRoot)) === "ready") {
    return true;
  }

  await vscode.window.showWarningMessage("Import Lens daemon is unavailable.");
  return false;
};

const reportRemoveResponse = async (
  logger: Pick<Logger, "info" | "warn">,
  scope: CacheClearScope,
  response: CacheRemoveResponse | null,
): Promise<void> => {
  if (!response) {
    await vscode.window.showWarningMessage("Import Lens cache removal did not return a result.");
    return;
  }

  if (response.error) {
    logger.warn(`Cache removal failed for ${scope}: ${response.error}`);
    await vscode.window.showWarningMessage(`Import Lens cache removal failed: ${response.error}`);
    return;
  }

  const removed = response.removed.length;
  const failed = response.failed.length;
  const message = cacheRemovalToast(scope, removed, failed);

  if (failed > 0) {
    logger.warn(message);
    await vscode.window.showWarningMessage(message);
    return;
  }

  logger.info(message);
  await vscode.window.showInformationMessage(message);
};

const notifyAfterRemove = (
  scope: CacheClearScope,
  response: CacheRemoveResponse | null,
  afterMutation?: () => void,
): void => {
  if (!response || response.error) {
    return;
  }
  // Registry and "everything" mutate shared/derived state (registry hints,
  // resolvers, L1/graph, generation bump) even when they remove zero shards, so
  // a refresh is always warranted; the shard-only scopes refresh only when they
  // actually removed a shard.
  const mutated = scope === "registry" || scope === "everything" || response.removed.length > 0;
  if (mutated) {
    afterMutation?.();
  }
};
