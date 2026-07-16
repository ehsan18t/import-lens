import type { ImportLensConfig } from "../config.js";
import type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";
import type { PackageJsonDependencySectionName } from "../ipc/protocol.js";
import {
  bytesForCompression,
  formatBytes,
  labelForCompression,
  type MeasuredSizes,
  measuredSizes,
} from "./format.js";
import type { PackageJsonPrimaryTone, PackageJsonSuffixTone } from "./packageJsonHintVisuals.js";
import {
  isNativeBinaryOnlyResult,
  isNativeBinaryResult,
  isTypesOnlyResult,
} from "./resultDiagnostics.js";

export type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";

const freshReleaseWindowMs = 24 * 60 * 60 * 1000;

export interface PackageJsonHintParts {
  readonly primary: string;
  readonly primaryTone: PackageJsonPrimaryTone;
  readonly suffix: string | null;
  readonly suffixTone: PackageJsonSuffixTone | null;
}

export const isFreshLatestRelease = (
  registryHint: PackageJsonDependencyHintState["registryHint"],
  now: number = Date.now(),
): boolean => {
  if (!registryHint?.latestPublishedAt) {
    return false;
  }

  const publishedAt = Date.parse(registryHint.latestPublishedAt);

  return (
    Number.isFinite(publishedAt) && publishedAt <= now && now - publishedAt < freshReleaseWindowMs
  );
};

export const packageJsonDependencyVersionStatusSuffix = (
  state: PackageJsonDependencyHintState,
): Pick<PackageJsonHintParts, "suffix" | "suffixTone"> => {
  const { registryHint } = state;

  if (!registryHint?.latestVersion) {
    return state.registryHintRefreshStatus === "stale"
      ? { suffix: "registry stale", suffixTone: "stale" }
      : { suffix: null, suffixTone: null };
  }

  let suffix: string | null = null;
  let suffixTone: PackageJsonSuffixTone | null = null;

  if (state.status === "missing") {
    suffix = `install ${registryHint.latestVersion}`;
    suffixTone = "install";
  } else if (registryHint.isLatest === true) {
    suffix = "latest";
    suffixTone = "latest";
  } else if (registryHint.isLatest === false) {
    suffix = `update ${registryHint.latestVersion}`;
    suffixTone = "update";
  }

  if (state.registryHintRefreshStatus === "stale") {
    return {
      suffix: suffix ? `stale · ${suffix}` : "registry stale",
      suffixTone: "stale",
    };
  }

  return { suffix, suffixTone };
};

export const packageJsonDependencyVersionStatusLabel = (
  state: PackageJsonDependencyHintState,
  now: number = Date.now(),
): string | null => {
  const { suffix } = packageJsonDependencyVersionStatusSuffix(state);

  if (!suffix) {
    return null;
  }

  if (
    state.status === "missing" ||
    !state.registryHint ||
    !isFreshLatestRelease(state.registryHint, now)
  ) {
    return suffix;
  }

  return `✦ ${suffix}`;
};

export const packageJsonDependencyHintParts = (
  state: PackageJsonDependencyHintState,
  config: ImportLensConfig,
): PackageJsonHintParts => {
  if (state.status === "loading") {
    return {
      primary: "checking...",
      primaryTone: "neutral",
      suffix: null,
      suffixTone: null,
    };
  }

  if (state.status === "missing") {
    return {
      primary: "not installed",
      primaryTone: "neutral",
      ...packageJsonDependencyVersionStatusSuffix(state),
    };
  }

  // "Is there a size?" — a dependency the engine could not measure reads "unavailable", which is
  // what it is, rather than borrowing a number from somewhere else.
  const sizes = measuredSizes(state.result);

  if (state.status === "unavailable" || !state.result || !sizes) {
    return {
      primary: "unavailable",
      primaryTone: "unavailable",
      ...packageJsonDependencyVersionStatusSuffix(state),
    };
  }

  if (isTypesOnlyResult(state.result)) {
    return {
      primary: "types only",
      primaryTone: "neutral",
      ...packageJsonDependencyVersionStatusSuffix(state),
    };
  }

  // No importable JS entry — the tool is a native binary. A badge, not a byte size, the same shape
  // as "types only" (B3).
  if (isNativeBinaryOnlyResult(state.result)) {
    return {
      primary: "native binary only",
      primaryTone: "neutral",
      ...packageJsonDependencyVersionStatusSuffix(state),
    };
  }

  const confidencePrefix = state.result.confidence === "low" ? "~" : "";
  // The JS entry resolved but the tool is backed by a native binary: keep the measured size and
  // flag it, so a thin shim's number is not read as the whole cost (B3).
  const nativeBinarySuffix = isNativeBinaryResult(state.result) ? " · native binary" : "";
  const primary = `${confidencePrefix}${formatBytes(bytesForCompression(sizes, config.compression))} ${labelForCompression(config.compression)}${nativeBinarySuffix}`;

  return {
    primary,
    primaryTone: "neutral",
    ...packageJsonDependencyVersionStatusSuffix(state),
  };
};

export const packageJsonDependencyHintLabel = (
  state: PackageJsonDependencyHintState,
  config: ImportLensConfig,
): string => {
  const parts = packageJsonDependencyHintParts(state, config);

  return [parts.primary, parts.suffix].filter((part): part is string => Boolean(part)).join(" · ");
};

/**
 * What the reader must be told about the figure beside "N/M measured", because it is not what the
 * package costs and nothing on the line said so.
 *
 * Each dependency is measured **alone**, against an otherwise-empty app, and the section adds those
 * numbers up. But `react-dom` pulls `react`'s whole graph and `@mui/material` pulls emotion's, and
 * in any real build those graphs are shared — so the figure counts them at every site. It is a
 * **Combined Import Cost** (ADR-0004): an upper bound that ranks a section's dependencies and
 * apportions blame among them, and never a size.
 */
export const packageJsonCombinedImportCostNote =
  "Combined Import Cost: each dependency measured on its own, as if nothing else were installed, then added up. Dependencies that share a graph are counted at every site, so this is an upper bound — not what this package costs.";

export const packageJsonSectionSummaryLabel = (
  section: PackageJsonDependencySectionName,
  states: readonly PackageJsonDependencyHintState[],
  config: ImportLensConfig,
): string | null => {
  const sectionStates = states.filter((state) => state.section === section);

  if (sectionStates.length === 0) {
    return null;
  }

  // "N/M measured" means exactly that: a dependency with no size is not one of the N, and its bytes
  // are not in the figure. `!state.result?.error` counted a fabricated size as a measurement.
  const measuredSections = sectionStates
    .map((state): [PackageJsonDependencyHintState, MeasuredSizes | null] => [
      state,
      state.status === "ready" ? measuredSizes(state.result) : null,
    ])
    .filter((pair): pair is [PackageJsonDependencyHintState, MeasuredSizes] => pair[1] !== null);
  const measuredStates = measuredSections.map(([state]) => state);
  // A sum of standalone Import Costs — a **Combined Import Cost**, and the label says so. Rendered
  // bare, "141.2 kB br" beside "3/3 measured" reads as *what this package costs*, which is a number
  // this product does not compute and this one is not (ADR-0004).
  const combinedImportCostBytes = measuredSections.reduce(
    (sum, [, sizes]) => sum + bytesForCompression(sizes, config.compression),
    0,
  );
  const missingCount = sectionStates.filter((state) => state.status === "missing").length;
  const unavailableCount = sectionStates.filter(
    (state) =>
      state.status === "unavailable" ||
      (state.status === "ready" && measuredSizes(state.result) === null),
  ).length;
  const loadingCount = sectionStates.filter((state) => state.status === "loading").length;

  if (
    measuredStates.length === 0 &&
    missingCount === 0 &&
    unavailableCount === 0 &&
    loadingCount > 0
  ) {
    return `${loadingCount} checking...`;
  }

  const parts = [
    `${measuredStates.length}/${sectionStates.length} measured`,
    `${formatBytes(combinedImportCostBytes)} ${labelForCompression(config.compression)} combined`,
  ];

  if (missingCount > 0) {
    parts.push(`${missingCount} not installed`);
  }

  if (unavailableCount > 0) {
    parts.push(`${unavailableCount} unavailable`);
  }

  return parts.join(" · ");
};
