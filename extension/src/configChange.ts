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

  if (event.affectsConfiguration("importLens.registryCacheMaxSizeMB")) {
    return "daemonRestart";
  }

  if (event.affectsConfiguration("importLens.enabled")) {
    return "reanalyze";
  }

  // The status bar keeps only the figure it rendered, in the format it rendered it in. The File
  // Cost in another format is in the daemon's file-size response, so a new format needs a new read.
  if (event.affectsConfiguration("importLens.compression")) {
    return "reanalyze";
  }

  if (event.affectsConfiguration("importLens")) {
    return "uiOnly";
  }

  return "uiOnly";
};
