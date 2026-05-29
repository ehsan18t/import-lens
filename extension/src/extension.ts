import * as vscode from "vscode";
import { AnalysisStore } from "./analysis/state.js";
import { getImportLensConfig, languageSelector } from "./config.js";
import { DaemonManager } from "./daemon/manager.js";
import { DocumentAnalysisController } from "./listener.js";
import { ImportLensLogger } from "./logger.js";
import { registerNodeModulesWatchers } from "./watcher.js";
import { ImportLensCodeLensProvider } from "./ui/codelens.js";
import { DecorationController } from "./ui/decorations.js";
import { ImportLensInlayHintsProvider, tooltipForResult } from "./ui/inlayHints.js";
import { showReport } from "./ui/report.js";
import { StatusBarController } from "./ui/statusbar.js";
import type { ImportResult } from "./ipc/protocol.js";

let daemon: DaemonManager | undefined;

export const activate = async (context: vscode.ExtensionContext): Promise<void> => {
  const config = getImportLensConfig();
  const logger = new ImportLensLogger(config.logLevel);
  const store = new AnalysisStore();
  const statusBar = new StatusBarController();
  const decorations = new DecorationController(store);
  const inlayHints = new ImportLensInlayHintsProvider(store);
  const codeLens = new ImportLensCodeLensProvider(store);

  daemon = new DaemonManager(context, logger);
  context.subscriptions.push(logger, store, statusBar, decorations, inlayHints, codeLens, daemon);
  context.subscriptions.push(vscode.languages.registerInlayHintsProvider(languageSelector, inlayHints));
  context.subscriptions.push(vscode.languages.registerCodeLensProvider(languageSelector, codeLens));

  const analysis = new DocumentAnalysisController(context, store, daemon, logger, statusBar);
  context.subscriptions.push(analysis);

  context.subscriptions.push(
    vscode.commands.registerCommand("importLens.showLogs", () => logger.show()),
    vscode.commands.registerCommand("importLens.clearCache", () => {
      daemon?.invalidateAll();
      const editor = vscode.window.activeTextEditor;

      if (editor) {
        void analysis.analyze(editor.document);
      }
    }),
    vscode.commands.registerCommand("importLens.showReport", () => showReport(context, store)),
    vscode.commands.registerCommand("importLens.showImportDetails", (result: ImportResult) => {
      void vscode.window.showInformationMessage(tooltipForResult(result).value);
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (!event.affectsConfiguration("importLens")) {
        return;
      }

      const nextConfig = getImportLensConfig();
      logger.setLevel(nextConfig.logLevel);
      decorations.refreshActiveEditor();
    }),
  );

  registerNodeModulesWatchers(context, daemon);
  const state = await daemon.start();
  statusBar.setStatus(state === "ready" ? "ready" : "unavailable");

  if (vscode.window.activeTextEditor) {
    analysis.schedule(vscode.window.activeTextEditor.document);
  }
};

export const deactivate = async (): Promise<void> => {
  await daemon?.dispose();
};

