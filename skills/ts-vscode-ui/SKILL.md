---
name: ts-vscode-ui
description: "Implementing VS Code display modes: Inlay Hints (with undefined kind + tooltip), Code Lens, and end-of-line Decorations. Use when implementing extension/src/ui/ (FR-029–FR-034, FR-039)."
---

# Instructions

ImportLens features four display modes controlled by `importLens.display`: `minimal`, `standard`, `verbose` (using TextEditorDecorations) and `inlayHint` (using `InlayHintsProvider`).

## 1. Inlay Hints (FR-039) — CRITICAL DETAILS

When rendering inlay hints, do not modify the document. Insert `InlayHint` objects at the exact position _after_ the closing quote of the import specifier.

> [!IMPORTANT]
> **`kind` MUST be `undefined`** — do NOT use `InlayHintKind.Parameter` or `InlayHintKind.Type`.
> Using `Parameter` applies `editorInlayHint.parameterForeground`/`parameterBackground` theme colors.
> Using `Type` applies `editorInlayHint.typeForeground`/`typeBackground` theme colors.
> An `undefined` kind falls through to the generic `editorInlayHint.foreground`/`editorInlayHint.background`, which theme authors expect for custom inlay hints.

Each `InlayHint` MUST set `tooltip` to a `MarkdownString` containing the full size breakdown:

- Raw bytes, minified bytes
- All three compressed sizes (gzip, brotli, zstd)
- `side_effects` status
- `is_cjs` indicator

```typescript
import * as vscode from "vscode";

export class ImportLensInlayProvider implements vscode.InlayHintsProvider {
  provideInlayHints(
    document: vscode.TextDocument,
    range: vscode.Range,
    token: vscode.CancellationToken,
  ): vscode.ProviderResult<vscode.InlayHint[]> {
    const hints: vscode.InlayHint[] = [];

    // For each import result:
    const position = new vscode.Position(line, endOfSpecifierQuote);
    const hint = new vscode.InlayHint(
      position,
      ` ${formattedSize}`,
      undefined, // <-- NOT InlayHintKind.Parameter!
    );

    // Add detailed tooltip
    hint.tooltip = new vscode.MarkdownString(
      `**${specifier}** @ ${version}\n\n` +
        `| Metric | Size |\n|---|---|\n` +
        `| Raw | ${rawBytes} |\n` +
        `| Minified | ${minifiedBytes} |\n` +
        `| Gzip | ${gzipBytes} |\n` +
        `| Brotli | ${brotliBytes} |\n` +
        `| Zstd | ${zstdBytes} |\n\n` +
        (sideEffects ? "⚠️ Has side effects\n" : "") +
        (isCjs ? "📦 CJS (approximate)\n" : ""),
    );

    hints.push(hint);
    return hints;
  }
}
```

## 2. Text Editor Decorations (minimal/standard/verbose)

For decoration-based modes, the size string appears after the line:

```typescript
const decorationType = vscode.window.createTextEditorDecorationType({
  after: {
    color: new vscode.ThemeColor("editorCodeLens.foreground"),
    margin: "0 0 0 2em",
  },
});
```

Display format examples:

- **minimal**: `1.5 kB`
- **standard**: `5.3 kB → 1.5 kB (br)`
- **verbose**: `5.3 kB min · 1.8 kB gz · 1.5 kB br · 1.6 kB zstd`

## 3. Warning Indicators (FR-031)

When `side_effects: true` or `truly_treeshakeable: false`, display a warning indicator (e.g., `⚠️`) next to the size decoration.

## 4. Loading Indicators (FR-032)

Display a loading indicator (e.g., `⏳ computing...`) next to imports that are currently being computed (cache miss in progress).

## 5. Settings Reactivity (FR-034)

Changing the `importLens.compression` setting must immediately update all currently visible decorations without requiring a file change or editor reload. Listen to `vscode.workspace.onDidChangeConfiguration`.

## Rules

- **Do not** modify the actual text of the document to show sizes.
- Use `vscode.ThemeColor` instead of hardcoded hex values to support user theme changes.
- **NEVER** use `InlayHintKind.Parameter` or `InlayHintKind.Type`. Always use `undefined`.
- Register the InlayHintsProvider for language selectors: `javascript`, `typescript`, `typescriptreact`, `javascriptreact`.
