import type { ImportLensConfig } from "./config.js";
import type { DaemonState } from "./daemon/transport.js";
import { supportedLanguageIds } from "./languages.js";

export interface RefreshableDocument {
  readonly languageId: string;
  readonly uri: {
    readonly scheme: string;
    toString(): string;
  };
}

export type ConfigRefreshMode = "reanalyze" | "uiOnly";

export interface ImportLensRefreshActions<TDocument extends RefreshableDocument> {
  schedule(document: TDocument): void;
  clear(uri: TDocument["uri"]): void;
  refreshDecorations(): void;
  refreshBudgetDiagnostics(): void;
  refreshInlayHints(): void;
  refreshCodeLens(): void;
  refreshPackageJsonHints(): void;
  reapplyInsights?(): void;
}

export const refreshVisibleImportLensDocuments = <TDocument extends RefreshableDocument>(
  documents: Iterable<TDocument>,
  config: ImportLensConfig,
  actions: ImportLensRefreshActions<TDocument>,
  mode: ConfigRefreshMode = "reanalyze",
): void => {
  if (mode === "reanalyze") {
    for (const document of documents) {
      if (!supportedLanguageIds.has(document.languageId) || document.uri.scheme !== "file") {
        continue;
      }

      if (config.enabled) {
        actions.schedule(document);
      } else {
        actions.clear(document.uri);
      }
    }
  } else {
    actions.reapplyInsights?.();
  }

  actions.refreshDecorations();
  actions.refreshBudgetDiagnostics();
  actions.refreshInlayHints();
  actions.refreshCodeLens();
  actions.refreshPackageJsonHints();
};

export interface DaemonStateTransitionActions {
  setStatus(state: DaemonState): void;
  prewarmPackageJson(): void;
  refreshPackageJsonHints(): void;
  refreshPackageJsonDecorations(): void;
  reanalyzeDocuments(): void;
}

export const applyDaemonStateTransition = (
  state: DaemonState,
  actions: DaemonStateTransitionActions,
): void => {
  actions.setStatus(state);

  if (state !== "ready") {
    return;
  }

  actions.prewarmPackageJson();
  actions.refreshPackageJsonHints();
  actions.refreshPackageJsonDecorations();
  // On automatic recovery the daemon-cleared document sizes must be recomputed;
  // the package.json refreshes above do not cover open source documents.
  actions.reanalyzeDocuments();
};
