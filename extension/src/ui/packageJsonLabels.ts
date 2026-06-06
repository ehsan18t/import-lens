import type { ImportLensConfig } from "../config.js";
import type { PackageJsonDependencySectionName } from "../guidance/packageJsonDependencies.js";
import type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";
import type { ImportResult } from "../ipc/protocol.js";
import { formatBytes, type CompressionFormat } from "./format.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";

export type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";

const latestVersionPart = (registryHint: PackageJsonDependencyHintState["registryHint"]): string | null =>
  registryHint?.latestVersion ? `latest ${registryHint.latestVersion}` : null;

const packageJsonDependencyLabel = (
  primary: string,
  registryHint: PackageJsonDependencyHintState["registryHint"],
): string =>
  [primary, latestVersionPart(registryHint)]
    .filter((part): part is string => Boolean(part))
    .join(" · ");

const selectedCompressionBytes = (
  result: ImportResult,
  compression: CompressionFormat,
): number => {
  if (compression === "gzip") {
    return result.gzip_bytes;
  }

  if (compression === "zstd") {
    return result.zstd_bytes;
  }

  return result.brotli_bytes;
};

const selectedCompressionLabel = (compression: CompressionFormat): string => {
  if (compression === "gzip") {
    return "gz";
  }

  if (compression === "zstd") {
    return "zstd";
  }

  return "br";
};

export const packageJsonDependencyHintLabel = (
  state: PackageJsonDependencyHintState,
  config: ImportLensConfig,
): string => {
  if (state.status === "loading") {
    return packageJsonDependencyLabel("checking...", state.registryHint);
  }

  if (state.status === "missing") {
    return packageJsonDependencyLabel("not installed", state.registryHint);
  }

  if (state.status === "unavailable" || !state.result || state.result.error) {
    return packageJsonDependencyLabel("unavailable", state.registryHint);
  }

  if (isTypesOnlyResult(state.result)) {
    return packageJsonDependencyLabel("types only", state.registryHint);
  }

  const confidencePrefix = state.result.confidence === "low" ? "~" : "";
  const primary = `${confidencePrefix}${formatBytes(selectedCompressionBytes(state.result, config.compression))} ${selectedCompressionLabel(config.compression)}`;

  return packageJsonDependencyLabel(primary, state.registryHint);
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

  const measuredStates = sectionStates.filter((state) =>
    state.status === "ready" && state.result && !state.result.error);
  const totalBytes = measuredStates.reduce(
    (sum, state) => sum + selectedCompressionBytes(state.result!, config.compression),
    0,
  );
  const missingCount = sectionStates.filter((state) => state.status === "missing").length;
  const unavailableCount = sectionStates.filter((state) =>
    state.status === "unavailable" || (state.status === "ready" && Boolean(state.result?.error))).length;
  const loadingCount = sectionStates.filter((state) => state.status === "loading").length;

  if (measuredStates.length === 0 && missingCount === 0 && unavailableCount === 0 && loadingCount > 0) {
    return `${loadingCount} checking...`;
  }

  const parts = [
    `${measuredStates.length}/${sectionStates.length} measured`,
    `${formatBytes(totalBytes)} ${selectedCompressionLabel(config.compression)}`,
  ];

  if (missingCount > 0) {
    parts.push(`${missingCount} not installed`);
  }

  if (unavailableCount > 0) {
    parts.push(`${unavailableCount} unavailable`);
  }

  return parts.join(" · ");
};
