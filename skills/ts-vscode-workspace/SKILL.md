---
name: ts-vscode-workspace
description: "VS Code FileSystemWatcher for node_modules cache invalidation and OutputChannel-based diagnostic logging. Use when implementing extension/src/watcher.ts and extension/src/logger.ts (FR-027, FR-040, FR-041)."
---

# Instructions

The extension monitors `node_modules` for package version changes and logs operations via the VS Code API.

## 1. FileSystemWatcher (Cache Invalidation — FR-027)

Watch for `package.json` changes in `node_modules` to trigger daemon cache eviction:

```typescript
import * as vscode from "vscode";

export function setupWatcher(ipcClient: IpcClient): vscode.Disposable {
  // IMPORTANT: Use single-level glob, NOT recursive (**/node_modules/**/package.json)
  const watcher = vscode.workspace.createFileSystemWatcher(
    "**/node_modules/*/package.json",
  );

  const handleChange = (uri: vscode.Uri) => {
    const pkgName = extractPackageName(uri);
    if (pkgName) {
      ipcClient.send({ type: "cache_invalidate", package: pkgName });
    }
  };

  watcher.onDidChange(handleChange);
  watcher.onDidCreate(handleChange);
  watcher.onDidDelete(handleChange);

  return watcher;
}

function extractPackageName(uri: vscode.Uri): string | null {
  // Extract package name from path like:
  //   .../node_modules/lodash-es/package.json -> "lodash-es"
  //   .../node_modules/@babel/core/package.json -> "@babel/core"
  const parts = uri.fsPath.split(/[\\/]/);
  const nmIdx = parts.lastIndexOf("node_modules");
  if (nmIdx === -1 || nmIdx + 1 >= parts.length) return null;

  const next = parts[nmIdx + 1];
  if (next.startsWith("@") && nmIdx + 2 < parts.length) {
    return `${next}/${parts[nmIdx + 2]}`;
  }
  return next;
}
```

> [!WARNING]
> Do NOT use `**/node_modules/**/package.json` (double-star after node_modules). This would watch deeply nested package.json files inside transitive dependencies, generating excessive file watcher events and potentially exhausting OS limits. Use `**/node_modules/*/package.json` (single-star) for direct dependencies only.

## 2. node_modules Deletion Handling

When the `node_modules` folder is deleted entirely, the watcher fires delete events. The extension must:

- Evict all cache entries for all packages in that tree.
- Update all affected decorations to "Package not found".

## 3. Diagnostic Logging (FR-040)

Do not use `console.log`. Create a dedicated `OutputChannel`:

```typescript
const logger = vscode.window.createOutputChannel("ImportLens");

type LogLevel = "error" | "warn" | "info" | "debug";
const LOG_PRIORITY: Record<LogLevel, number> = {
  error: 0,
  warn: 1,
  info: 2,
  debug: 3,
};

let currentLevel: LogLevel = "error";

export function setLogLevel(level: LogLevel): void {
  currentLevel = level;
}

export function log(level: LogLevel, message: string): void {
  if (LOG_PRIORITY[level] > LOG_PRIORITY[currentLevel]) return;
  logger.appendLine(
    `[${new Date().toISOString()}] [${level.toUpperCase()}] ${message}`,
  );
}
```

## 4. Show Logs Command (FR-041)

Register a command `ImportLens: Show Logs` that focuses the output channel:

```typescript
vscode.commands.registerCommand("importLens.showLogs", () => {
  logger.show(true); // preserveFocus = true
});
```

This command must be available at all times, regardless of the extension's current operating tier.

## Rules

- Use `vscode.workspace.createFileSystemWatcher` — NEVER use the Rust `notify` crate for watching from the extension host side.
- Filter log verbosity using the `importLens.logLevel` configuration setting.
- Log messages MUST include ISO 8601 timestamps and severity level.
- Listen to `vscode.workspace.onDidChangeConfiguration` to react to `importLens.logLevel` changes at runtime.
