import type { ConfidenceLevel, ImportResult } from "../ipc/protocol.js";
import type { ImportRuntime } from "../ipc/protocol.js";
import type { InlineHintTone } from "./inlineHintVisuals.js";
import { isTypesOnlyResult } from "./resultDiagnostics.js";

export type DisplayMode = "minimal" | "standard" | "verbose" | "inlayHint";

export type CompressionFormat = "brotli" | "gzip" | "zstd" | "all";

export interface FormatOptions {
  display: DisplayMode;
  compression: CompressionFormat;
  showWarnings: boolean;
}

const compressionLabels = {
  brotli: "br",
  gzip: "gz",
  zstd: "zstd",
} as const;

const bytesForCompression = (result: ImportResult, compression: CompressionFormat): number => {
  if (compression === "gzip") {
    return result.gzip_bytes;
  }

  if (compression === "zstd") {
    return result.zstd_bytes;
  }

  return result.brotli_bytes;
};

const labelForCompression = (compression: CompressionFormat): string =>
  compression === "all" ? "br" : compressionLabels[compression];

export const formatBytes = (bytes: number): string => {
  if (bytes < 1000) {
    return `${bytes} B`;
  }

  return `${(bytes / 1000).toFixed(1)} kB`;
};

const formatWarningSuffix = (result: ImportResult, showWarnings: boolean, runtime: ImportRuntime): string => {
  const runtimeSuffix = runtime === "server" ? " · server" : "";

  if (isTypesOnlyResult(result)) {
    return `${runtimeSuffix} · types only`;
  }

  if (!showWarnings) {
    return runtimeSuffix;
  }

  const warningTags = [];

  if (result.is_cjs) {
    warningTags.push("CJS");
  }

  if (warningTags.length > 0) {
    return `${runtimeSuffix} · ${warningTags.join(" · ")}`;
  }

  return runtimeSuffix;
};

const confidencePrefix = (result: ImportResult): string =>
  result.confidence === "low" ? "~" : "";

export const importSizePrimaryTone = (confidence: ConfidenceLevel): InlineHintTone => {
  if (confidence === "medium") {
    return "sizeMedium";
  }

  if (confidence === "low") {
    return "sizeLow";
  }

  return "size";
};

export const importHintTagLabels = (
  result: ImportResult,
  showWarnings: boolean,
  runtime: ImportRuntime,
): string[] => {
  const tags: string[] = [];

  if (runtime === "server") {
    tags.push("server");
  }

  if (isTypesOnlyResult(result)) {
    tags.push("types only");
  }

  if (showWarnings && result.is_cjs) {
    tags.push("CJS");
  }

  return tags;
};

export const formatImportSizePrimary = (
  result: ImportResult,
  options: FormatOptions,
  runtime: ImportRuntime = "component",
): string => {
  if (result.error) {
    return "Size unavailable";
  }

  if (options.display === "verbose" || options.compression === "all") {
    return `${confidencePrefix(result)}${formatBytes(result.brotli_bytes)} br · ${formatBytes(result.gzip_bytes)} gz · ${formatBytes(result.zstd_bytes)} zstd · ${formatBytes(result.minified_bytes)} min`;
  }

  const compressedBytes = bytesForCompression(result, options.compression);
  const compressed = formatBytes(compressedBytes);
  const label = labelForCompression(options.compression);

  if (options.display === "minimal" || options.display === "inlayHint") {
    return `${confidencePrefix(result)}${compressed} ${label}`;
  }

  return `${confidencePrefix(result)}${compressed} ${label} · ${formatBytes(result.minified_bytes)} min`;
};

export const formatImportSize = (
  result: ImportResult,
  options: FormatOptions,
  runtime: ImportRuntime = "component",
): string => {
  if (result.error) {
    return "Size unavailable";
  }

  return `${formatImportSizePrimary(result, options, runtime)}${formatWarningSuffix(result, options.showWarnings, runtime)}`;
};
