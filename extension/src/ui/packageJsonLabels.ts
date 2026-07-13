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
import { isTypesOnlyResult } from "./resultDiagnostics.js";

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

  const confidencePrefix = state.result.confidence === "low" ? "~" : "";
  const primary = `${confidencePrefix}${formatBytes(bytesForCompression(sizes, config.compression))} ${labelForCompression(config.compression)}`;

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

export const packageJsonSectionSummaryLabel = (
  section: PackageJsonDependencySectionName,
  states: readonly PackageJsonDependencyHintState[],
  config: ImportLensConfig,
): string | null => {
  const sectionStates = states.filter((state) => state.section === section);

  if (sectionStates.length === 0) {
    return null;
  }

  // "N/M measured" means exactly that now: a dependency with no size is not one of the N, and its
  // bytes are not in the total. `!state.result?.error` counted a fabricated size as a measurement.
  const measuredSections = sectionStates
    .map((state): [PackageJsonDependencyHintState, MeasuredSizes | null] => [
      state,
      state.status === "ready" ? measuredSizes(state.result) : null,
    ])
    .filter((pair): pair is [PackageJsonDependencyHintState, MeasuredSizes] => pair[1] !== null);
  const measuredStates = measuredSections.map(([state]) => state);
  const totalBytes = measuredSections.reduce(
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
    `${formatBytes(totalBytes)} ${labelForCompression(config.compression)}`,
  ];

  if (missingCount > 0) {
    parts.push(`${missingCount} not installed`);
  }

  if (unavailableCount > 0) {
    parts.push(`${unavailableCount} unavailable`);
  }

  return parts.join(" · ");
};
