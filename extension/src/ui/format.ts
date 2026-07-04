import type { ConfidenceLevel, ImportResult, ImportRuntime } from "../ipc/protocol.js";
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

export interface CompressionByteSizes {
  brotli_bytes: number;
  gzip_bytes: number;
  zstd_bytes: number;
}

export const bytesForCompression = (
  sizes: CompressionByteSizes,
  compression: CompressionFormat,
): number => {
  if (compression === "gzip") {
    return sizes.gzip_bytes;
  }

  if (compression === "zstd") {
    return sizes.zstd_bytes;
  }

  return sizes.brotli_bytes;
};

export const labelForCompression = (compression: CompressionFormat): string =>
  compression === "all" ? "br" : compressionLabels[compression];

export const formatBytes = (bytes: number): string => {
  if (bytes < 1000) {
    return `${bytes} B`;
  }

  return `${(bytes / 1000).toFixed(1)} kB`;
};

const confidencePrefix = (result: ImportResult): string => (result.confidence === "low" ? "~" : "");

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

export const formatImportSizePrimary = (result: ImportResult, options: FormatOptions): string => {
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
