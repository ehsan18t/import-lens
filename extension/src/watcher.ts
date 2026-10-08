import * as vscode from "vscode";
import type { DaemonManager } from "./daemon/manager.js";
import type { Logger } from "./logging/types.js";
import {
  createNodeModulesInvalidationBuffer,
  nodeModulesDeletionPath,
} from "./watcherInvalidation.js";

/**
 * The files the daemon memoizes and cannot see change on its own.
 *
 * `node_modules/*&#47;package.json` is an install or an uninstall. The `tsconfig` / `jsconfig` globs
 * are the workspace's **alias table** — the one thing that tells a path alias apart from a package
 * that is not installed. They were not watched, so the table the daemon parsed on its first
 * resolution was the table it used until it died: a developer who applied the repair the SRS
 * prescribes for an unrecognized alias (add the `paths` entry) saw the file stay a floor forever.
 *
 * The `*` in the config globs is what catches `tsconfig.app.json`, where the Vue and Astro
 * scaffolds actually keep `paths`. A Vite, webpack or Rollup config is an alias table as well. Configs under `node_modules` are dropped downstream
 * (`isWorkspaceConfigPath`) — they belong to a dependency's own build and an install would queue
 * thousands of them.
 */
const watchedPatterns = [
  "**/node_modules/*/package.json",
  "**/node_modules/@*/*/package.json",
  "**/tsconfig*.json",
  "**/jsconfig*.json",
  "**/{vite,webpack,rollup}.config.{js,mjs,cjs,ts,mts,cts}",
];

/** Deletes only: a removed folder is reported once, for the folder (`nodeModulesDeletionPath`). */
const deletedFolderPatterns = ["**/node_modules", "**/node_modules/*", "**/node_modules/@*/*"];

export const registerNodeModulesWatchers = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "info">,
  onInvalidated?: () => void,
): void => {
  const invalidationBuffer = createNodeModulesInvalidationBuffer(daemon, {
    logger,
    onInvalidated,
  });
  const queue = (uri: vscode.Uri): void => invalidationBuffer.queue(uri.fsPath);

  for (const pattern of watchedPatterns) {
    const watcher = vscode.workspace.createFileSystemWatcher(pattern);
    watcher.onDidCreate(queue, undefined, context.subscriptions);
    watcher.onDidChange(queue, undefined, context.subscriptions);
    watcher.onDidDelete(queue, undefined, context.subscriptions);
    context.subscriptions.push(watcher);
  }

  const queueDeletedFolder = (uri: vscode.Uri): void => {
    const invalidated = nodeModulesDeletionPath(uri.fsPath);

    if (invalidated) {
      invalidationBuffer.queue(invalidated);
    }
  };

  for (const pattern of deletedFolderPatterns) {
    const watcher = vscode.workspace.createFileSystemWatcher(pattern, true, true, false);
    watcher.onDidDelete(queueDeletedFolder, undefined, context.subscriptions);
    context.subscriptions.push(watcher);
  }

  context.subscriptions.push(invalidationBuffer);
};
