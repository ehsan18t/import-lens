import * as vscode from "vscode";
import { getImportLensConfig } from "../config.js";
import type {
  PackageJsonAnalysisController,
  PackageJsonDependencyAnalysisState,
} from "../guidance/packageJsonAnalysis.js";
import type { PackageJsonDependencySection } from "../ipc/protocol.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { shouldShowPackageJsonDecorations } from "./displayGuards.js";
import { InlineHintDecorationController } from "./inlineHintDecorationController.js";
import {
  type AnchoredInlineHint,
  inlineHintDecorationLayers,
  inlineHintLanes,
} from "./inlineHintDecorationTypes.js";
import { packageJsonDependencyHintAnchorCharacter } from "./packageJsonDecorationAnchor.js";
import {
  packageJsonHintSegments,
  packageJsonSectionSummarySegment,
} from "./packageJsonHintSegments.js";
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

export class PackageJsonDecorationController extends InlineHintDecorationController {
  readonly #analysis: PackageJsonAnalysisController;

  constructor(analysis: PackageJsonAnalysisController) {
    super(analysis);
    this.#analysis = analysis;
  }

  refreshEditor(editor: vscode.TextEditor): void {
    const config = getImportLensConfig();

    if (
      !shouldShowPackageJsonDecorations(config) ||
      editor.document.uri.scheme !== "file" ||
      !isPackageJsonPath(editor.document.fileName)
    ) {
      this.decorationPool.clearEditor(editor);
      return;
    }

    const states = this.#analysis.get(editor.document.uri);
    const sections = this.#analysis.sections(editor.document.uri);
    const hints = [
      ...sections.flatMap(
        (section) => this.hintForSection(editor.document, section, states, config) ?? [],
      ),
      ...states.map((state) => this.hintForState(editor.document, state, config)),
    ];

    this.decorationPool.applyToEditor(editor, inlineHintLanes(hints));
  }

  private hintForState(
    document: vscode.TextDocument,
    state: PackageJsonDependencyAnalysisState,
    config: ReturnType<typeof getImportLensConfig>,
  ): AnchoredInlineHint {
    const line = lineAtClamped(document, state.entry.valueRange.end.line);
    const anchor = new vscode.Position(
      line.lineNumber,
      packageJsonDependencyHintAnchorCharacter(line.text),
    );
    const parts = packageJsonDependencyHintParts(state, config);
    const tooltip = tooltipForPackageJsonState(state, config, document.uri.toString());

    return {
      anchor,
      layers: inlineHintDecorationLayers(packageJsonHintSegments(parts, config), anchor, tooltip),
    };
  }

  private hintForSection(
    document: vscode.TextDocument,
    section: PackageJsonDependencySection,
    states: readonly PackageJsonDependencyAnalysisState[],
    config: ReturnType<typeof getImportLensConfig>,
  ): AnchoredInlineHint | null {
    const label = packageJsonSectionSummaryLabel(section.section, states, config);

    if (!label) {
      return null;
    }

    const line = lineAtClamped(document, section.objectRange.start.line);
    const anchor = line.range.end;
    const sectionStates = states.filter((state) => state.section === section.section);

    return {
      anchor,
      layers: inlineHintDecorationLayers(
        [packageJsonSectionSummarySegment(label)],
        anchor,
        tooltipForPackageJsonSectionSummary(
          label,
          sectionStates,
          config,
          document.uri.toString(),
          section.section,
        ),
      ),
    };
  }
}

// The ranges come from the analysis of an earlier text, and lines may have been deleted since:
// `lineAt` past the end throws, which would abandon the whole refresh.
const lineAtClamped = (document: vscode.TextDocument, line: number): vscode.TextLine =>
  document.lineAt(Math.max(0, Math.min(line, document.lineCount - 1)));

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
  const trustedCommands = packageJsonDependencyTooltipTrustedCommands(state, config, {
    packageJsonUri,
  });

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
