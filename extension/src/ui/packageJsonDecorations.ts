import * as vscode from "vscode";
import type { PackageJsonAnalysisController, PackageJsonDependencyAnalysisState } from "../guidance/packageJsonAnalysis.js";
import type { PackageJsonDependencySection } from "../guidance/packageJsonDependencies.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowPackageJsonDecorations } from "./displayGuards.js";
import {
  packageJsonDependencyHintParts,
  packageJsonSectionSummaryLabel,
} from "./packageJsonLabels.js";
import { packageJsonDependencyTooltipMarkdown } from "./packageJsonTooltip.js";
import { tooltipForMessage } from "./tooltip.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { packageJsonDependencyHintAnchorCharacter } from "./packageJsonDecorationAnchor.js";
import {
  packageJsonHintDecorationGroups,
  packageJsonSectionSummaryDecorationOptions,
} from "./packageJsonDecorationSegments.js";

export class PackageJsonDecorationController implements vscode.Disposable {
  readonly #analysis: PackageJsonAnalysisController;
  readonly #primaryDecoration: vscode.TextEditorDecorationType;
  readonly #suffixDecoration: vscode.TextEditorDecorationType;
  readonly #subscription: vscode.Disposable;

  constructor(analysis: PackageJsonAnalysisController) {
    this.#analysis = analysis;
    this.#primaryDecoration = vscode.window.createTextEditorDecorationType({
      rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
    });
    this.#suffixDecoration = vscode.window.createTextEditorDecorationType({
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
      editor.setDecorations(this.#primaryDecoration, []);
      editor.setDecorations(this.#suffixDecoration, []);
      return;
    }

    const states = this.#analysis.get(editor.document.uri);
    const sections = this.#analysis.sections(editor.document.uri);
    const primaryDecorations: vscode.DecorationOptions[] = [];
    const suffixDecorations: vscode.DecorationOptions[] = [];

    for (const section of sections) {
      const option = this.decorationForSection(editor.document, section, states, config);

      if (option) {
        primaryDecorations.push(option);
      }
    }

    for (const state of states) {
      const groups = this.decorationGroupsForState(editor.document, state, config);
      primaryDecorations.push(...groups.primary);
      suffixDecorations.push(...groups.suffix);
    }

    editor.setDecorations(this.#primaryDecoration, primaryDecorations);
    editor.setDecorations(this.#suffixDecoration, suffixDecorations);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#primaryDecoration.dispose();
    this.#suffixDecoration.dispose();
  }

  private decorationGroupsForState(
    document: vscode.TextDocument,
    state: PackageJsonDependencyAnalysisState,
    config: ReturnType<typeof getImportLensConfig>,
  ): ReturnType<typeof packageJsonHintDecorationGroups> {
    const line = document.lineAt(state.entry.valueRange.end.line);
    const position = new vscode.Position(
      line.lineNumber,
      packageJsonDependencyHintAnchorCharacter(line.text),
    );
    const parts = packageJsonDependencyHintParts(state, config);
    const tooltip = tooltipForPackageJsonState(state);

    return packageJsonHintDecorationGroups(parts, position, config, tooltip);
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

    return packageJsonSectionSummaryDecorationOptions(
      label,
      position,
      tooltipForMessage("ImportLens dependency summary", label),
    );
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
