import * as vscode from "vscode";
import type { AnalysisStore } from "../analysis/state.js";
import { getImportLensConfig } from "../config.js";
import type { ImportResult } from "../ipc/protocol.js";
import { formatImportSize } from "./format.js";

export class ImportLensInlayHintsProvider implements vscode.InlayHintsProvider, vscode.Disposable {
  readonly #store: AnalysisStore;
  readonly #onDidChangeInlayHints = new vscode.EventEmitter<void>();
  readonly #subscription: vscode.Disposable;

  readonly onDidChangeInlayHints: vscode.Event<void> = this.#onDidChangeInlayHints.event;

  constructor(store: AnalysisStore) {
    this.#store = store;
    this.#subscription = this.#store.onDidChange(() => this.#onDidChangeInlayHints.fire());
  }

  provideInlayHints(document: vscode.TextDocument): vscode.InlayHint[] {
    const config = getImportLensConfig();

    if (config.display !== "inlayHint") {
      return [];
    }

    return this.#store
      .get(document.uri)
      .filter((state) => state.status === "ready" && Boolean(state.result))
      .map((state) => {
        const result = state.result as ImportResult;
        const hint = new vscode.InlayHint(
          new vscode.Position(state.detected.quoteEnd.line, state.detected.quoteEnd.character),
          formatImportSize(result, config),
          undefined,
        );
        hint.tooltip = tooltipForResult(result);
        return hint;
      });
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#onDidChangeInlayHints.dispose();
  }
}

export const tooltipForResult = (result: ImportResult): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**${result.specifier}**\n\n`);
  tooltip.appendMarkdown(`Raw: ${result.raw_bytes} B\n\n`);
  tooltip.appendMarkdown(`Minified: ${result.minified_bytes} B\n\n`);
  tooltip.appendMarkdown(`Gzip: ${result.gzip_bytes} B\n\n`);
  tooltip.appendMarkdown(`Brotli: ${result.brotli_bytes} B\n\n`);
  tooltip.appendMarkdown(`Zstd: ${result.zstd_bytes} B\n\n`);
  tooltip.appendMarkdown(`Side effects: ${result.side_effects ? "yes" : "no"}\n\n`);
  tooltip.appendMarkdown(`CJS: ${result.is_cjs ? "yes" : "no"}`);
  return tooltip;
};
