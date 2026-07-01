import type * as vscode from "vscode";

export type ConfigChangeKind = "uiOnly" | "reanalyze" | "daemonRestart";

export const classifyImportLensConfigChange = (
  event: vscode.ConfigurationChangeEvent,
): ConfigChangeKind => {
  if (event.affectsConfiguration("importLens.enableDiskCache")) {
    return "daemonRestart";
  }

  if (event.affectsConfiguration("importLens.cacheMaxSizeMB")) {
    return "daemonRestart";
  }

  if (event.affectsConfiguration("importLens.cacheMaxAgeDays")) {
    return "daemonRestart";
  }

  if (event.affectsConfiguration("importLens.enabled")) {
    return "reanalyze";
  }

  if (event.affectsConfiguration("importLens")) {
    return "uiOnly";
  }

  return "uiOnly";
};
