import type { ImportLensConfig } from "../config.js";
import type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";
import {
  isFreshLatestRelease,
  packageJsonDependencyVersionStatusLabel,
} from "./packageJsonLabels.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";
import {
  copyDiagnosticsMarkdown,
  conservativeSizingMarkdown,
  importResultSizeMarkdown,
} from "./tooltipMarkdown.js";

export interface PackageJsonDependencyTooltipState extends PackageJsonDependencyHintState {
  message?: string;
}

const registryDetailsMarkdown = (
  state: PackageJsonDependencyTooltipState,
): string[] => {
  const details: string[] = [];
  const versionStatus = packageJsonDependencyVersionStatusLabel(state);

  if (state.installedVersion) {
    details.push(`Installed version: ${state.installedVersion}`);
  }

  if (state.registryHint?.latestVersion) {
    details.push(`Latest version: ${state.registryHint.latestVersion}`);
  }

  if (versionStatus) {
    details.push(`Version status: ${versionStatus}`);
  }

  if (state.registryHint?.latestPublishedAt) {
    details.push(`Latest published: ${state.registryHint.latestPublishedAt}`);
  }

  if (isFreshLatestRelease(state.registryHint)) {
    details.push("✦ New release under 24h");
  }

  return details;
};

export const packageJsonDependencyTooltipMarkdown = (
  state: PackageJsonDependencyTooltipState,
  config: Pick<ImportLensConfig, "compression">,
): string => {
  const parts: string[] = [`**${state.name}**`];

  if (state.status === "ready" && state.result && !state.result.error) {
    if (isTypesOnlyResult(state.result)) {
      parts.push("Type-only package: yes");
    } else {
      parts.push(importResultSizeMarkdown(state.result, config.compression));
      const conservativeSizing = conservativeSizingMarkdown(state.result);

      if (conservativeSizing) {
        parts.push(conservativeSizing);
      }
    }
  } else if (state.status === "ready" && state.result?.error) {
    parts.push("ImportLens could not compute this dependency size.");
    parts.push(state.result.error);
  } else if (state.message) {
    parts.push(state.message);
  }

  const registryDetails = registryDetailsMarkdown(state);

  if (registryDetails.length > 0) {
    parts.push([
      "**Package version**",
      ...registryDetails.map((detail) => `- ${detail}`),
    ].join("\n"));
  }

  if (state.result?.diagnostics.length) {
    parts.push(copyDiagnosticsMarkdown(state.result));
  }

  return parts.filter(Boolean).join("\n\n");
};
