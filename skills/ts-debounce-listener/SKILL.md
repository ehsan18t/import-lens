---
name: ts-debounce-listener
description: "Document change listener with 300ms debounce, import extraction, version resolution, and BatchRequest dispatch. Use when implementing extension/src/listener.ts (FR-004–FR-009, FR-012)."
---

# Instructions

The listener is the core orchestrator of the extension host's request lifecycle. It fires on document changes, extracts imports, resolves versions, and dispatches a single batched request.

## 1. Document Change Handler

Register a listener for text document changes with a 300ms debounce:

```typescript
import * as vscode from "vscode";

const DEBOUNCE_MS = 300;
let debounceTimer: ReturnType<typeof setTimeout> | undefined;

export function registerListener(
  extractImports: (text: string, path: string) => ExtractedImport[],
  resolveVersion: (
    specifier: string,
    docDir: string,
  ) => Promise<ResolvedPackage | null>,
  ipcClient: IpcClient,
  context: vscode.ExtensionContext,
): void {
  const disposable = vscode.workspace.onDidChangeTextDocument((event) => {
    const doc = event.document;

    // Only handle supported languages
    if (
      ![
        "javascript",
        "typescript",
        "typescriptreact",
        "javascriptreact",
      ].includes(doc.languageId)
    )
      return;

    if (debounceTimer) clearTimeout(debounceTimer);

    debounceTimer = setTimeout(() => {
      processDocument(doc, extractImports, resolveVersion, ipcClient);
    }, DEBOUNCE_MS);
  });

  context.subscriptions.push(disposable);
}
```

## 2. Request Pipeline

After debounce fires:

```typescript
async function processDocument(
  doc: vscode.TextDocument,
  extractImports: ExtractImportsFn,
  resolveVersion: ResolveVersionFn,
  ipcClient: IpcClient,
): Promise<void> {
  // 1. Extract imports using oxc-parser (synchronous, fast)
  const rawImports = extractImports(doc.getText(), doc.uri.fsPath);

  // 2. Resolve versions for all node_modules imports (parallel)
  const docDir = path.dirname(doc.uri.fsPath);
  const resolved = await Promise.all(
    rawImports.map(async (imp) => {
      const pkg = await resolveVersion(imp.specifier, docDir);
      if (!pkg) return null;
      return { ...imp, package: pkg.name, version: pkg.version };
    }),
  );

  // 3. Filter out unresolvable imports
  const validImports = resolved.filter(Boolean);
  if (validImports.length === 0) return;

  // 4. Send a SINGLE BatchRequest (FR-012)
  ipcClient.sendBatchRequest({
    version: 1,
    active_document_path: doc.uri.fsPath,
    imports: validImports,
  });

  // 5. Show loading indicators while waiting for response (FR-032)
  showLoadingIndicators(doc, validImports);
}
```

## 3. Response Handling

When the `BatchResponse` arrives:

1. Check the request ID to discard stale responses (FR-013).
2. Map each `ImportResult` back to its line in the document.
3. Apply decorations or inlay hints based on the active display mode.

## 4. First-Open Trigger

The listener should also trigger on `onDidOpenTextDocument` for the initial file open, not just changes:

```typescript
vscode.workspace.onDidOpenTextDocument((doc) => {
  if (isSupportedLanguage(doc)) {
    processDocument(doc, extractImports, resolveVersion, ipcClient);
  }
});
```

## Rules

- All imports from a single debounce cycle must be sent as ONE `BatchRequest`, never one per import.
- The debounce timer must be EXACTLY 300ms (SRS §3.4).
- Do NOT block the extension host main thread. All I/O is async (NFR-001).
- If the daemon is not connected (degraded mode), skip the request and leave existing decorations unchanged.
