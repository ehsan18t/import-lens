import * as vscode from "vscode";
import { sanitizeBudgets, type ImportLensBudgets } from "./analysis/budgets.js";
import type { CompressionFormat, DisplayMode } from "./ui/format.js";
import type { LogLevel } from "./ipc/protocol.js";
import { defaultLogLevel } from "./loggerCore.js";

export type InlineRenderer = "colored" | "native";

export interface ImportLensConfig {
  enabled: boolean;
  display: DisplayMode;
  inlineRenderer: InlineRenderer;
  compression: CompressionFormat;
  debounceMs: number;
  showWarnings: boolean;
  useCodeLens: boolean;
  enableDiskCache: boolean;
  logLevel: LogLevel;
  budgets: ImportLensBudgets;
}

export const getImportLensConfig = (): ImportLensConfig => {
  const config = vscode.workspace.getConfiguration("importLens");

  return {
    enabled: config.get("enabled", true),
    display: config.get<DisplayMode>("display", "inlayHint"),
    inlineRenderer: config.get<InlineRenderer>("inlineRenderer", "colored"),
    compression: config.get<CompressionFormat>("compression", "brotli"),
    debounceMs: config.get("debounceMs", 300),
    showWarnings: config.get("showWarnings", true),
    useCodeLens: config.get("useCodeLens", false),
    enableDiskCache: config.get("enableDiskCache", true),
    logLevel: config.get<LogLevel>("logLevel", defaultLogLevel),
    budgets: sanitizeBudgets(config.get("budgets", {})),
  };
};
