import * as vscode from "vscode";
import type { PackageJsonAnalysisController, PackageJsonDependencyAnalysisState } from "../guidance/packageJsonAnalysis.js";
import type { PackageJsonDependencySection } from "../guidance/packageJsonDependencies.js";
import { isPackageJsonPath } from "../prewarm/packageJsonHelpers.js";
import { getImportLensConfig } from "../config.js";
import {
  packageJsonDependencyHintLabel,
  packageJsonSectionSummaryLabel,
} from "./packageJsonLabels.js";
import { packageJsonDependencyTooltipMarkdown } from "./packageJsonTooltip.js";
import { tooltipForMessage } from "./tooltip.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";

export const packageJsonDocumentSelector: vscode.DocumentSelector = [
  { language: "json", scheme: "file", pattern: "**/package.json" },
  { language: "jsonc", scheme: "file", pattern: "**/package.json" },
];

export class PackageJsonDependencyInlayHintsProvider implements vscode.InlayHintsProvider, vscode.Disposable {
  readonly #analysis: PackageJsonAnalysisController;
  readonly #onDidChangeInlayHints = new vscode.EventEmitter<void>();
  readonly #subscription: vscode.Disposable;

  readonly onDidChangeInlayHints: vscode.Event<void> = this.#onDidChangeInlayHints.event;

  constructor(analysis: PackageJsonAnalysisController) {
    this.#analysis = analysis;
    this.#subscription = this.#analysis.onDidChange(() => this.#onDidChangeInlayHints.fire());
  }

  provideInlayHints(document: vscode.TextDocument): vscode.InlayHint[] {
    const config = getImportLensConfig();

    if (!config.enabled || document.uri.scheme !== "file" || !isPackageJsonPath(document.fileName)) {
      return [];
    }

    const states = this.#analysis.get(document.uri);
    const sections = this.#analysis.sections(document.uri);

    return [
      ...sections
        .map((section) => this.summaryHintForSection(section, states, config))
        .filter((hint): hint is vscode.InlayHint => Boolean(hint)),
      ...states.map((state) => this.hintForState(state, config)),
    ];
  }

  refresh(): void {
    this.#onDidChangeInlayHints.fire();
  }

  dispose(): void {
    this.#subscription.dispose();
    this.#onDidChangeInlayHints.dispose();
  }

  private hintForState(
    state: PackageJsonDependencyAnalysisState,
    config: ReturnType<typeof getImportLensConfig>,
  ): vscode.InlayHint {
    const label = packageJsonDependencyHintLabel(state, config);
    const labelPart = new vscode.InlayHintLabelPart(label);
    labelPart.tooltip = tooltipForPackageJsonState(state);

    if (state.status === "ready" && state.result) {
      labelPart.command = {
        title: "Show Import Details",
        command: "importLens.showImportDetails",
        arguments: [state.result, "component"],
      };
    }

    const hint = new vscode.InlayHint(
      new vscode.Position(state.entry.valueRange.end.line, state.entry.valueRange.end.character),
      [labelPart],
      undefined,
    );
    hint.paddingLeft = true;
    return hint;
  }

  private summaryHintForSection(
    section: PackageJsonDependencySection,
    states: readonly PackageJsonDependencyAnalysisState[],
    config: ReturnType<typeof getImportLensConfig>,
  ): vscode.InlayHint | null {
    const label = packageJsonSectionSummaryLabel(section.section, states, config);

    if (!label) {
      return null;
    }

    const labelPart = new vscode.InlayHintLabelPart(label);
    labelPart.tooltip = tooltipForMessage("ImportLens dependency summary", label);
    const hint = new vscode.InlayHint(
      new vscode.Position(section.objectRange.start.line, section.objectRange.start.character + 1),
      [labelPart],
      undefined,
    );
    hint.paddingLeft = true;
    return hint;
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
