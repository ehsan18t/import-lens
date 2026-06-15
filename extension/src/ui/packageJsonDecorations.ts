import * as vscode from "vscode";
import type { PackageJsonAnalysisController, PackageJsonDependencyAnalysisState } from "../guidance/packageJsonAnalysis.js";
import type { PackageJsonDependencySection } from "../guidance/packageJsonDependencies.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowPackageJsonDecorations } from "./displayGuards.js";
import {
  packageJsonDependencyHintLabel,
  packageJsonSectionSummaryLabel,
} from "./packageJsonLabels.js";
import { packageJsonDependencyTooltipMarkdown } from "./packageJsonTooltip.js";
import { tooltipForMessage } from "./tooltip.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";

export class PackageJsonDecorationController implements vscode.Disposable {
  readonly #analysis: PackageJsonAnalysisController;
  readonly #decoration: vscode.TextEditorDecorationType;
  readonly #subscription: vscode.Disposable;

  constructor(analysis: PackageJsonAnalysisController) {
    this.#analysis = analysis;
    this.#decoration = vscode.window.createTextEditorDecorationType({
      after: {
        margin: "0 0 0 0.75rem",
        fontStyle: "italic",
      },
      rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
    });
    this.#subscription = this.#analysis.onDidChange((uri) => this.refreshUri(uri));
  }

  refreshVisibleEditors(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      this.refreshEditor(editor);
    }
  }

  refreshUri(uri: vscode.Uri): void {
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.toString() === uri.toString()) {
        this.refreshEditor(editor);
      }
    }
  }

  refreshEditor(editor: vscode.TextEditor): void {
    const config = getImportLensConfig();

    if (
      !shouldShowPackageJsonDecorations(config)
      || editor.document.uri.scheme !== "file"
      || !isPackageJsonPath(editor.document.fileName)
    ) {
      editor.setDecorations(this.#decoration, []);
      return;
    }

    const states = this.#analysis.get(editor.document.uri);
    const sections = this.#analysis.sections(editor.document.uri);
    const decorations = [
      ...sections
        .map((section) => this.decorationForSection(editor.document, section, states, config))
        .filter((value): value is vscode.DecorationOptions => Boolean(value)),
      ...states
        .map((state) => this.decorationForState(state, config))
        .filter((value): value is vscode.DecorationOptions => Boolean(value)),
    ];

    editor.setDecorations(this.#decoration, decorations);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#decoration.dispose();
  }

  private decorationForState(
    state: PackageJsonDependencyAnalysisState,
    config: ReturnType<typeof getImportLensConfig>,
  ): vscode.DecorationOptions | null {
    const position = new vscode.Position(
      state.entry.valueRange.end.line,
      state.entry.valueRange.end.character,
    );
    const label = packageJsonDependencyHintLabel(state, config);
    const tooltip = tooltipForPackageJsonState(state);

    return {
      range: new vscode.Range(position, position),
      hoverMessage: tooltip,
      renderOptions: {
        after: {
          contentText: ` ${label}`,
          fontStyle: "italic",
        },
      },
    };
  }

  private decorationForSection(
    document: vscode.TextDocument,
    section: PackageJsonDependencySection,
    states: readonly PackageJsonDependencyAnalysisState[],
    config: ReturnType<typeof getImportLensConfig>,
  ): vscode.DecorationOptions | null {
    const label = packageJsonSectionSummaryLabel(section.section, states, config);

    if (!label) {
      return null;
    }

    const line = document.lineAt(section.objectRange.start.line);
    const position = line.range.end;

    return {
      range: new vscode.Range(position, position),
      hoverMessage: tooltipForMessage("ImportLens dependency summary", label),
      renderOptions: {
        after: {
          contentText: ` ${label}`,
          fontStyle: "italic",
        },
      },
    };
  }
}

const tooltipForPackageJsonState = (
  state: PackageJsonDependencyAnalysisState,
): vscode.MarkdownString | undefined => {
  if (state.status === "loading") {
    return undefined;
  }

  const tooltip = new vscode.MarkdownString(
    packageJsonDependencyTooltipMarkdown(state, getImportLensConfig()),
    true,
  );

  if (state.result?.diagnostics.length) {
    tooltip.isTrusted = { enabledCommands: [copyImportDiagnosticsCommand] };
  }

  return tooltip;
};
