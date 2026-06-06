import type { ImportLensConfig } from "../config.js";
import type { PackageJsonDependencySectionName } from "../guidance/packageJsonDependencies.js";
import type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";
import type { ImportResult } from "../ipc/protocol.js";
import { formatBytes, formatImportSize, type CompressionFormat } from "./format.js";

export type { PackageJsonDependencyHintState } from "../guidance/packageJsonState.js";

const registryHintSuffix = (registryHint: PackageJsonDependencyHintState["registryHint"]): string =>
  registryHint?.deprecated ? " · deprecated" : "";

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
    return "checking...";
  }

  if (state.status === "missing") {
    return `not installed${registryHintSuffix(state.registryHint)}`;
  }

  if (state.status === "unavailable" || !state.result || state.result.error) {
    return `unavailable${registryHintSuffix(state.registryHint)}`;
  }

  return `${formatImportSize(state.result, config)}${registryHintSuffix(state.registryHint)}`;
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
