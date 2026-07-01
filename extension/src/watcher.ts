import * as vscode from "vscode";
import type { DaemonManager } from "./daemon/manager.js";
import type { Logger } from "./logging/types.js";
import { createNodeModulesInvalidationBuffer } from "./watcherInvalidation.js";

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

  for (const pattern of ["**/node_modules/*/package.json", "**/node_modules/@*/*/package.json"]) {
    const watcher = vscode.workspace.createFileSystemWatcher(pattern);
    watcher.onDidCreate(queue, undefined, context.subscriptions);
    watcher.onDidChange(queue, undefined, context.subscriptions);
    watcher.onDidDelete(queue, undefined, context.subscriptions);
    context.subscriptions.push(watcher);
  }

  context.subscriptions.push(invalidationBuffer);
};
