import path from "node:path";
import * as vscode from "vscode";
import type { DaemonManager } from "./daemon/manager.js";
import { getPackageName } from "./imports/specifier.js";
import type { Logger } from "./logging/types.js";
import { flushNodeModulesInvalidations } from "./watcherActions.js";

export const registerNodeModulesWatchers = (
  context: vscode.ExtensionContext,
  daemon: DaemonManager,
  logger: Pick<Logger, "info">,
  onInvalidated?: () => void,
): void => {
  const pending = new Set<string>();
  let timer: NodeJS.Timeout | undefined;

  const flush = (): void => {
    const packages = [...pending];
    pending.clear();
    flushNodeModulesInvalidations(packages, daemon, onInvalidated, logger);
  };

  const queue = (uri: vscode.Uri): void => {
    const packageName = packageNameFromPackageJsonPath(uri.fsPath);

    if (!packageName) {
      logger.info("node_modules package.json changed outside a package path; invalidating entire cache.");
      daemon.invalidateAll();
      onInvalidated?.();
      return;
    }

    pending.add(packageName);

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

const packageNameFromPackageJsonPath = (packageJsonPath: string): string | null => {
  const normalized = packageJsonPath.split(path.sep).join("/");
  const marker = "/node_modules/";
  const index = normalized.lastIndexOf(marker);

  if (index === -1) {
    return null;
  }

  const afterNodeModules = normalized.slice(index + marker.length).replace(/\/package\.json$/u, "");
  return getPackageName(afterNodeModules);
};
