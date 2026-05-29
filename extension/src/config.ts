import * as vscode from "vscode";
import type { CompressionFormat, DisplayMode } from "./ui/format.js";
import type { LogLevel } from "./ipc/protocol.js";

export interface ImportLensConfig {
  enabled: boolean;
  display: DisplayMode;
  compression: CompressionFormat;
  debounceMs: number;
  showWarnings: boolean;
  useCodeLens: boolean;
  enableDiskCache: boolean;
  logLevel: LogLevel;
}

export const getImportLensConfig = (): ImportLensConfig => {
  const config = vscode.workspace.getConfiguration("importLens");

  return {
    enabled: config.get("enabled", true),
    display: config.get<DisplayMode>("display", "inlayHint"),
    compression: config.get<CompressionFormat>("compression", "brotli"),
    debounceMs: config.get("debounceMs", 300),
    showWarnings: config.get("showWarnings", true),
    useCodeLens: config.get("useCodeLens", false),
    enableDiskCache: config.get("enableDiskCache", true),
    logLevel: config.get<LogLevel>("logLevel", "error"),
  };
};
