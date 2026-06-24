import * as vscode from "vscode";
import type { DaemonManager } from "./daemon/manager.js";
import type { Logger } from "./logging/types.js";

export const registerNodeModulesWatchers = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "info">,
  onInvalidated?: () => void,
): void => {
  const pending = new Set<string>();
  let timer: NodeJS.Timeout | undefined;

  const flush = (): void => {
    const packageJsonPaths = [...pending];
    pending.clear();
    daemon.nodeModulesChanged(packageJsonPaths);
    logger.info(`Queued ${packageJsonPaths.length} node_modules package.json invalidation(s).`);
    onInvalidated?.();
  };

  const queue = (uri: vscode.Uri): void => {
    pending.add(uri.fsPath);

    if (timer) {
      clearTimeout(timer);
    }

    timer = setTimeout(flush, 250);
  };

  for (const pattern of ["**/node_modules/*/package.json", "**/node_modules/@*/*/package.json"]) {
    const watcher = vscode.workspace.createFileSystemWatcher(pattern);
    watcher.onDidCreate(queue, undefined, context.subscriptions);
    watcher.onDidChange(queue, undefined, context.subscriptions);
    watcher.onDidDelete(queue, undefined, context.subscriptions);
    context.subscriptions.push(watcher);
  }
};
