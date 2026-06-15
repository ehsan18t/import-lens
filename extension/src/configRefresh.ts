import type { ImportLensConfig } from "./config.js";
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
