import * as vscode from "vscode";
import { AnalysisStore } from "./analysis/state.js";
import { refreshVisibleImportLensDocuments } from "./configRefresh.js";
import { getImportLensConfig, type ImportLensConfig } from "./config.js";
import { DaemonManager } from "./daemon/manager.js";
import { PackageJsonAnalysisController } from "./guidance/packageJsonAnalysis.js";
import { DocumentAnalysisController } from "./listener.js";
import { languageSelector } from "./languages.js";
import { ImportLensLogger } from "./logger.js";
import { registerNodeModulesWatchers } from "./watcher.js";
import { registerPackageJsonPrewarm } from "./prewarm/packageJson.js";
import { BudgetDiagnosticsController } from "./ui/budgetDiagnostics.js";
import { ImportLensCodeLensProvider } from "./ui/codelens.js";
import { compareImports, compareImportsCommand } from "./ui/compareImports.js";
import { ImportMemberCompletionProvider } from "./ui/completions.js";
import { DecorationController } from "./ui/decorations.js";
import { copyImportDiagnosticsCommand, formatImportDiagnostics } from "./ui/diagnostics.js";
import { ImportLensHoverProvider } from "./ui/hoverProvider.js";
import { ImportLensInlayHintsProvider } from "./ui/inlayHints.js";
import { showBundleImpactHistory, showCurrentFileSize } from "./ui/currentFileSize.js";
import { showNamedExportCandidates, showNamedExportCandidatesCommand } from "./ui/namedExportCandidates.js";
import { PackageJsonDependencyInlayHintsProvider, packageJsonDocumentSelector } from "./ui/packageJsonInlayHints.js";
import { showReport } from "./ui/report.js";
import { StatusBarController } from "./ui/statusbar.js";
import { tooltipForResult } from "./ui/tooltip.js";
import { TreeShakeCodeActionProvider } from "./ui/treeShakeActions.js";
import type { ImportResult } from "./ipc/protocol.js";
import type { DetectedImport, ImportRuntime } from "./imports/types.js";

let daemon: DaemonManager | undefined;

const copyImportDiagnostics = async (result: ImportResult): Promise<void> => {
  await vscode.env.clipboard.writeText(formatImportDiagnostics(result));
  void vscode.window.showInformationMessage("ImportLens diagnostics copied.");
};

const copySubstitutionSuggestion = async (
  currentSpecifier: string,
  replacementPackage: string,
  reason: string,
): Promise<void> => {
  await vscode.env.clipboard.writeText(`${currentSpecifier} -> ${replacementPackage}\n${reason}`);
  void vscode.window.showInformationMessage(`ImportLens alternative copied: ${replacementPackage}.`);
};

export const activate = async (context: vscode.ExtensionContext): Promise<void> => {
  const config = getImportLensConfig();
  const logger = new ImportLensLogger(config.logLevel);
  logger.info("ImportLens activation started.");
  const store = new AnalysisStore();
  const statusBar = new StatusBarController();
  const decorations = new DecorationController(store);
  const budgetDiagnostics = new BudgetDiagnosticsController(store);
  const inlayHints = new ImportLensInlayHintsProvider(store);
  const hoverProvider = new ImportLensHoverProvider(store);
  const codeLens = new ImportLensCodeLensProvider(store);
  const treeShakeActions = new TreeShakeCodeActionProvider(store);

  daemon = new DaemonManager(context, logger);
  const packageJsonAnalysis = new PackageJsonAnalysisController(context, daemon, logger);
  const packageJsonInlayHints = new PackageJsonDependencyInlayHintsProvider(packageJsonAnalysis);
  const completions = new ImportMemberCompletionProvider(daemon);
  context.subscriptions.push(logger, store, statusBar, decorations, budgetDiagnostics, inlayHints, codeLens, packageJsonAnalysis, packageJsonInlayHints, daemon);
  context.subscriptions.push(vscode.languages.registerInlayHintsProvider(languageSelector, inlayHints));
  context.subscriptions.push(vscode.languages.registerInlayHintsProvider(packageJsonDocumentSelector, packageJsonInlayHints));
  context.subscriptions.push(vscode.languages.registerHoverProvider(languageSelector, hoverProvider));
  context.subscriptions.push(vscode.languages.registerCodeLensProvider(languageSelector, codeLens));
  context.subscriptions.push(vscode.languages.registerCompletionItemProvider(languageSelector, completions, "{", ","));
  context.subscriptions.push(vscode.languages.registerCodeActionsProvider(languageSelector, treeShakeActions));

  const analysis = new DocumentAnalysisController(context, store, daemon, logger, statusBar);
  context.subscriptions.push(analysis);

  const refreshVisibleDocuments = (nextConfig: ImportLensConfig): void => {
    refreshVisibleImportLensDocuments(
      vscode.window.visibleTextEditors.map((editor) => editor.document),
      nextConfig,
      {
        schedule: (document) => analysis.schedule(document),
        clear: (uri) => store.clear(uri),
        refreshDecorations: () => decorations.refreshVisibleEditors(),
        refreshBudgetDiagnostics: () => budgetDiagnostics.refreshVisibleEditors(),
        refreshInlayHints: () => inlayHints.refresh(),
        refreshCodeLens: () => codeLens.refresh(),
        refreshPackageJsonHints: () => {
          packageJsonAnalysis.refreshVisibleDocuments();
          packageJsonInlayHints.refresh();
        },
      },
    );
  };

  context.subscriptions.push(
    vscode.commands.registerCommand("importLens.showLogs", () => logger.show()),
    vscode.commands.registerCommand("importLens.showCurrentFileSize", () => void showCurrentFileSize(context, daemon!, logger)),
    vscode.commands.registerCommand("importLens.showBundleImpactHistory", () => void showBundleImpactHistory(context)),
    vscode.commands.registerCommand("importLens.clearCache", () => {
      logger.info("Clearing ImportLens daemon cache.");
      daemon?.invalidateAll();
      const editor = vscode.window.activeTextEditor;

      if (editor) {
        void analysis.analyze(editor.document);
      }
    }),
    vscode.commands.registerCommand("importLens.showReport", () => void showReport(context, daemon!)),
    vscode.commands.registerCommand(compareImportsCommand, (initialSpecifier?: string) => void compareImports(daemon!, initialSpecifier)),
    vscode.commands.registerCommand("importLens.copySubstitutionSuggestion", copySubstitutionSuggestion),
    vscode.commands.registerCommand("importLens.showImportDetails", async (result: ImportResult, runtime: ImportRuntime = "component") => {
      if (result.error) {
        const action = await vscode.window.showWarningMessage(
          "ImportLens could not compute this import size.",
          "Copy diagnostics",
        );

        if (action === "Copy diagnostics") {
          await copyImportDiagnostics(result);
        }

        return;
      }

      void vscode.window.showInformationMessage(tooltipForResult(result, runtime).value);
    }),
    vscode.commands.registerCommand(copyImportDiagnosticsCommand, async (result?: ImportResult) => {
      if (!result) {
        void vscode.window.showWarningMessage(
          "No ImportLens diagnostics are available for the active command context.",
        );
        return;
      }

      await copyImportDiagnostics(result);
    }),
    vscode.commands.registerCommand(showNamedExportCandidatesCommand, async (uri: vscode.Uri, detected: DetectedImport) => {
      await showNamedExportCandidates(daemon!, logger, uri, detected);
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (!event.affectsConfiguration("importLens")) {
        return;
      }

      const nextConfig = getImportLensConfig();
      logger.setLevel(nextConfig.logLevel);
      logger.info("ImportLens configuration changed; refreshing visible documents.");
      refreshVisibleDocuments(nextConfig);
    }),
  );

  registerNodeModulesWatchers(context, daemon, () => refreshVisibleDocuments(getImportLensConfig()));
  const state = await daemon.start();
  logger.info(`ImportLens daemon startup completed with state: ${state}.`);
  statusBar.setStatus(state === "ready" ? "ready" : "unavailable");
  registerPackageJsonPrewarm(context, daemon);

  if (vscode.window.activeTextEditor) {
    analysis.schedule(vscode.window.activeTextEditor.document);
  }
};

export const deactivate = async (): Promise<void> => {
  await daemon?.dispose();
};
