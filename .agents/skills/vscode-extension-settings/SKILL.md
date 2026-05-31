---
name: vscode-extension-settings
description: "All VS Code configuration settings contributed by ImportLens, including display modes, compression format, log level, and size thresholds. Use when implementing extension/src/config.ts and contributes.configuration in package.json (FR-029–FR-037)."
---

# Instructions

ImportLens contributes the following settings to VS Code's configuration system. These must be declared in `package.json` under `contributes.configuration`.

## 1. Settings Schema

```json
{
  "contributes": {
    "configuration": {
      "title": "ImportLens",
      "properties": {
        "importLens.display": {
          "type": "string",
          "enum": ["minimal", "standard", "verbose", "inlayHint"],
          "default": "standard",
          "description": "How import sizes are displayed.",
          "enumDescriptions": [
            "1.5 kB — primary compression format only",
            "5.3 kB → 1.5 kB (br) — minified + primary compression",
            "5.3 kB min · 1.8 kB gz · 1.5 kB br · 1.6 kB zstd — all formats",
            "Inline hint after the import specifier using the Inlay Hints API"
          ]
        },
        "importLens.compression": {
          "type": "string",
          "enum": ["gzip", "brotli", "zstd"],
          "default": "brotli",
          "description": "Primary compression format shown in minimal and standard display modes."
        },
        "importLens.sizeThreshold": {
          "type": "number",
          "default": 0,
          "description": "Minimum size in bytes before a decoration is shown. Set to 0 to show all."
        },
        "importLens.enableDiskCache": {
          "type": "boolean",
          "default": true,
          "description": "Persist computed sizes to disk using redb. Speeds up cold starts."
        },
        "importLens.logLevel": {
          "type": "string",
          "enum": ["error", "warn", "info", "debug"],
          "default": "error",
          "description": "Verbosity level for the ImportLens output channel."
        }
      }
    }
  }
}
```

## 2. Commands

Register these commands in `package.json`:

```json
{
  "contributes": {
    "commands": [
      {
        "command": "importLens.showLogs",
        "title": "ImportLens: Show Logs"
      },
      {
        "command": "importLens.showReport",
        "title": "ImportLens: Show Report"
      },
      {
        "command": "importLens.clearCache",
        "title": "ImportLens: Clear Cache"
      }
    ]
  }
}
```

## 3. Configuration Access in TypeScript

```typescript
import * as vscode from "vscode";

export function getConfig() {
  const config = vscode.workspace.getConfiguration("importLens");
  return {
    display: config.get<"minimal" | "standard" | "verbose" | "inlayHint">(
      "display",
      "standard",
    ),
    compression: config.get<"gzip" | "brotli" | "zstd">(
      "compression",
      "brotli",
    ),
    sizeThreshold: config.get<number>("sizeThreshold", 0),
    enableDiskCache: config.get<boolean>("enableDiskCache", true),
    logLevel: config.get<"error" | "warn" | "info" | "debug">(
      "logLevel",
      "error",
    ),
  };
}

// Listen for configuration changes (FR-034)
vscode.workspace.onDidChangeConfiguration((event) => {
  if (event.affectsConfiguration("importLens")) {
    const newConfig = getConfig();
    // Update display mode, compression, etc. without file reload
  }
});
```

## 4. Settings Reactivity (FR-034)

Changing `importLens.compression` must immediately update all visible decorations WITHOUT requiring a file change or editor reload. The extension must:

1. Listen to `onDidChangeConfiguration`.
2. Re-render existing cached results with the new format.

## Rules

- The default display mode is `standard`, NOT `minimal`.
- The default compression format is `brotli` (most common on CDNs in 2026).
- Always read settings through `vscode.workspace.getConfiguration()` — never hardcode defaults.
