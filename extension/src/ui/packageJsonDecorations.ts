import * as vscode from "vscode";
import type { PackageJsonAnalysisController, PackageJsonDependencyAnalysisState } from "../guidance/packageJsonAnalysis.js";
import type { PackageJsonDependencySection } from "../ipc/protocol.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { getImportLensConfig } from "../config.js";
import { shouldShowPackageJsonDecorations } from "./displayGuards.js";
import {
  packageJsonDependencyHintParts,
  packageJsonSectionSummaryLabel,
} from "./packageJsonLabels.js";
import {
  packageJsonDependencyTooltipMarkdown,
  packageJsonDependencyTooltipTrustedCommands,
  packageJsonSectionSummaryTooltipMarkdown,
  packageJsonSectionSummaryTooltipTrustedCommands,
} from "./packageJsonTooltip.js";
import { packageJsonDependencyHintAnchorCharacter } from "./packageJsonDecorationAnchor.js";
import {
  emptyInlineHintDecorationLayers,
  InlineHintSlotDecorationPool,
  inlineHintDecorationLayers,
  mergeInlineHintDecorationLayers,
} from "./inlineHintDecorationTypes.js";
import { packageJsonHintSegments, packageJsonSectionSummarySegment } from "./packageJsonHintSegments.js";

export class PackageJsonDecorationController implements vscode.Disposable {
  readonly #analysis: PackageJsonAnalysisController;
  readonly #decorationPool: InlineHintSlotDecorationPool;
  readonly #subscription: vscode.Disposable;

  constructor(analysis: PackageJsonAnalysisController) {
    this.#analysis = analysis;
    this.#decorationPool = new InlineHintSlotDecorationPool();
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
      this.#decorationPool.clearEditor(editor);
      return;
    }

    const states = this.#analysis.get(editor.document.uri);
    const sections = this.#analysis.sections(editor.document.uri);
    const layers = emptyInlineHintDecorationLayers();

    for (const section of sections) {
      const sectionLayers = this.decorationLayersForSection(editor.document, section, states, config);

      if (sectionLayers) {
        mergeInlineHintDecorationLayers(layers, sectionLayers);
      }
    }

    for (const state of states) {
      mergeInlineHintDecorationLayers(layers, this.decorationLayersForState(editor.document, state, config));
    }

    this.#decorationPool.applyToEditor(editor, layers);
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#decorationPool.dispose();
  }

  private decorationLayersForState(
    document: vscode.TextDocument,
    state: PackageJsonDependencyAnalysisState,
    config: ReturnType<typeof getImportLensConfig>,
  ): ReturnType<typeof inlineHintDecorationLayers> {
    const line = document.lineAt(state.entry.valueRange.end.line);
    const anchor = new vscode.Position(
      line.lineNumber,
      packageJsonDependencyHintAnchorCharacter(line.text),
    );
    const parts = packageJsonDependencyHintParts(state, config);
    const tooltip = tooltipForPackageJsonState(state, config, document.uri.toString());

    return inlineHintDecorationLayers(
      packageJsonHintSegments(parts, config),
      anchor,
      tooltip,
    );
  }

  private decorationLayersForSection(
    document: vscode.TextDocument,
    section: PackageJsonDependencySection,
    states: readonly PackageJsonDependencyAnalysisState[],
    config: ReturnType<typeof getImportLensConfig>,
  ): ReturnType<typeof inlineHintDecorationLayers> | null {
    const label = packageJsonSectionSummaryLabel(section.section, states, config);

    if (!label) {
      return null;
    }

    const line = document.lineAt(section.objectRange.start.line);
    const anchor = line.range.end;
    const sectionStates = states.filter((state) => state.section === section.section);

    return inlineHintDecorationLayers(
      [packageJsonSectionSummarySegment(label)],
      anchor,
      tooltipForPackageJsonSectionSummary(
        label,
        sectionStates,
        config,
        document.uri.toString(),
        section.section,
      ),
    );
  }
}

const tooltipForPackageJsonState = (
  state: PackageJsonDependencyAnalysisState,
  config: ReturnType<typeof getImportLensConfig>,
  packageJsonUri: string,
): vscode.MarkdownString | undefined => {
  if (state.status === "loading") {
    return undefined;
  }

  const tooltip = new vscode.MarkdownString(
    packageJsonDependencyTooltipMarkdown(state, config, { packageJsonUri }),
    true,
  );
  const trustedCommands = packageJsonDependencyTooltipTrustedCommands(
    state,
    config,
    { packageJsonUri },
  );

  if (trustedCommands.length > 0) {
    tooltip.isTrusted = { enabledCommands: trustedCommands };
  }

  return tooltip;
};

const tooltipForPackageJsonSectionSummary = (
  label: string,
  states: readonly PackageJsonDependencyAnalysisState[],
  config: ReturnType<typeof getImportLensConfig>,
  packageJsonUri: string,
  section: PackageJsonDependencySection["section"],
): vscode.MarkdownString => {
  const tooltip = new vscode.MarkdownString(
    packageJsonSectionSummaryTooltipMarkdown(label, states, config, {
      packageJsonUri,
      section,
    }),
    true,
  );
  const trustedCommands = packageJsonSectionSummaryTooltipTrustedCommands(config, {
    packageJsonUri,
    section,
  });

  if (trustedCommands.length > 0) {
    tooltip.isTrusted = { enabledCommands: trustedCommands };
  }

  return tooltip;
};
