import type { ImportLensConfig } from "../config.js";
import type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";
import type { PackageJsonDependencySectionName } from "../ipc/protocol.js";
import { copyImportDiagnosticsCommand } from "./diagnostics.js";
import { measuredSizes } from "./format.js";
import {
  isFreshLatestRelease,
  packageJsonDependencyVersionStatusLabel,
} from "./packageJsonLabels.js";
import {
  refreshPackageJsonRegistryHintCommand,
  refreshPackageJsonRegistryHintsCommand,
} from "./packageJsonRegistryCommands.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";
import {
  conservativeSizingMarkdown,
  copyDiagnosticsMarkdown,
  importResultSizeMarkdown,
} from "./tooltipMarkdown.js";

export interface PackageJsonDependencyTooltipState extends PackageJsonDependencyHintState {
  message?: string;
}

interface PackageJsonRegistryTooltipConfig {
  enableRegistryHints: boolean;
}

export interface PackageJsonTooltipActionOptions {
  packageJsonUri?: string;
  formatFetchedAt?: (timestamp: number) => string;
}

export interface PackageJsonSectionSummaryTooltipOptions extends PackageJsonTooltipActionOptions {
  section?: PackageJsonDependencySectionName;
}

const defaultFormatFetchedAt = (timestamp: number): string => new Date(timestamp).toLocaleString();

const commandArgs = (args: readonly unknown[]): string => encodeURIComponent(JSON.stringify(args));

const refreshPackageRegistryHintMarkdown = (
  state: PackageJsonDependencyTooltipState,
  options: PackageJsonTooltipActionOptions,
): string | null => {
  if (!options.packageJsonUri) {
    return null;
  }

  const args = commandArgs([options.packageJsonUri, state.name, state.installedVersion]);
  return `[$(sync) Refresh npm registry info](command:${refreshPackageJsonRegistryHintCommand}?${args})`;
};

const refreshPackageRegistryHintsMarkdown = (
  options: PackageJsonSectionSummaryTooltipOptions,
): string | null => {
  if (!options.packageJsonUri) {
    return null;
  }

  const args = commandArgs([options.packageJsonUri, options.section]);
  return `[$(sync) Refresh all npm registry info](command:${refreshPackageJsonRegistryHintsCommand}?${args})`;
};

const registryDetailsMarkdown = (
  state: PackageJsonDependencyTooltipState,
  options: PackageJsonTooltipActionOptions,
): string[] => {
  const details: string[] = [];
  const versionStatus = packageJsonDependencyVersionStatusLabel(state);
  const formatFetchedAt = options.formatFetchedAt ?? defaultFormatFetchedAt;

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

  if (typeof state.registryHint?.fetchedAt === "number") {
    details.push(`Registry info fetched: ${formatFetchedAt(state.registryHint.fetchedAt)}`);
  }

  if (isFreshLatestRelease(state.registryHint)) {
    details.push("✦ New release under 24h");
  }

  if (state.registryHintRefreshStatus === "stale") {
    details.push("$(warning) Showing cached registry data because the latest refresh failed");
  }

  if (state.registryHintRefreshError) {
    details.push(`Refresh error: ${state.registryHintRefreshError}`);
  }

  return details;
};

export const packageJsonDependencyTooltipMarkdown = (
  state: PackageJsonDependencyTooltipState,
  config: Pick<ImportLensConfig, "compression" | "enableRegistryHints">,
  options: PackageJsonTooltipActionOptions = {},
): string => {
  const parts: string[] = [`**${state.name}**`];
  // "Is there a size?", never "is there an error?" (ADR-0006, invariant 2). This branch renders one
  // — through `importResultSizeMarkdown`, which is why the guard that scans for the banned check
  // never saw this file: it names no size of its own. It asks the question correctly now, and the
  // guard discovers it by the RESULT it handles rather than by the words it happens to spell.
  const sizes = state.status === "ready" ? measuredSizes(state.result) : null;

  if (state.result && sizes) {
    if (isTypesOnlyResult(state.result)) {
      parts.push("Type-only package: yes");
    } else {
      parts.push(importResultSizeMarkdown(state.result, config.compression));
      const conservativeSizing = conservativeSizingMarkdown(state.result);

      if (conservativeSizing) {
        parts.push(conservativeSizing);
      }
    }
  } else if (state.status === "ready" && state.result) {
    parts.push("Import Lens could not compute this dependency size.");
    parts.push(state.result.error ?? "No size was produced for this dependency.");
  } else if (state.message) {
    parts.push(state.message);
  }

  const registryDetails = registryDetailsMarkdown(state, options);

  if (registryDetails.length > 0) {
    parts.push(
      ["**Package version**", ...registryDetails.map((detail) => `- ${detail}`)].join("\n"),
    );
  }

  const refreshAction = config.enableRegistryHints
    ? refreshPackageRegistryHintMarkdown(state, options)
    : null;

  if (refreshAction) {
    parts.push(refreshAction);
  }

  if (state.result?.diagnostics.length) {
    parts.push(copyDiagnosticsMarkdown(state.result));
  }

  return parts.filter(Boolean).join("\n\n");
};

export const packageJsonDependencyTooltipTrustedCommands = (
  state: PackageJsonDependencyTooltipState,
  config: PackageJsonRegistryTooltipConfig,
  options: PackageJsonTooltipActionOptions = {},
): string[] => {
  const commands: string[] = [];

  if (state.result?.diagnostics.length) {
    commands.push(copyImportDiagnosticsCommand);
  }

  if (config.enableRegistryHints && options.packageJsonUri) {
    commands.push(refreshPackageJsonRegistryHintCommand);
  }

  return commands;
};

const sectionFetchedAtMarkdown = (
  states: readonly PackageJsonDependencyHintState[],
  options: PackageJsonSectionSummaryTooltipOptions,
): string => {
  const fetchedTimes = states.flatMap((state) =>
    typeof state.registryHint?.fetchedAt === "number" ? [state.registryHint.fetchedAt] : [],
  );

  if (states.length === 0 || fetchedTimes.length !== states.length) {
    return "Some registry info has not been fetched yet";
  }

  const oldestFetchedAt = Math.min(...fetchedTimes);
  const formatFetchedAt = options.formatFetchedAt ?? defaultFormatFetchedAt;
  return `All registry info fetched since: ${formatFetchedAt(oldestFetchedAt)}`;
};

export const packageJsonSectionSummaryTooltipMarkdown = (
  label: string,
  states: readonly PackageJsonDependencyHintState[],
  config: PackageJsonRegistryTooltipConfig,
  options: PackageJsonSectionSummaryTooltipOptions = {},
): string => {
  const parts = ["**Import Lens dependency summary**", label];

  if (config.enableRegistryHints) {
    parts.push(sectionFetchedAtMarkdown(states, options));
    const refreshAction = refreshPackageRegistryHintsMarkdown(options);

    if (refreshAction) {
      parts.push(refreshAction);
    }
  }

  return parts.join("\n\n");
};

export const packageJsonSectionSummaryTooltipTrustedCommands = (
  config: PackageJsonRegistryTooltipConfig,
  options: PackageJsonSectionSummaryTooltipOptions = {},
): string[] =>
  config.enableRegistryHints && options.packageJsonUri
    ? [refreshPackageJsonRegistryHintsCommand]
    : [];
