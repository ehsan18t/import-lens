import type { ImportResult } from "../ipc/protocol.js";
import type { ImportRuntime } from "../imports/types.js";

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

  if (result.is_cjs) {
    return `${runtimeSuffix} · CJS`;
  }

  if (showWarnings && (result.side_effects || !result.truly_treeshakeable)) {
    return `${runtimeSuffix} · approximate`;
  }

  return runtimeSuffix;
};

export const formatImportSize = (
  result: ImportResult,
  options: FormatOptions,
  runtime: ImportRuntime = "component",
): string => {
  if (result.error) {
    return "unavailable";
  }

  if (options.display === "verbose" || options.compression === "all") {
    return `${formatBytes(result.brotli_bytes)} br · ${formatBytes(result.gzip_bytes)} gz · ${formatBytes(result.zstd_bytes)} zstd · ${formatBytes(result.minified_bytes)} min${formatWarningSuffix(result, options.showWarnings, runtime)}`;
  }

  const compressedBytes = bytesForCompression(result, options.compression);
  const compressed = formatBytes(compressedBytes);
  const label = labelForCompression(options.compression);
  const suffix = formatWarningSuffix(result, options.showWarnings, runtime);

  if (options.display === "minimal" || options.display === "inlayHint") {
    return `${compressed}${suffix}`;
  }

  return `${compressed} ${label} · ${formatBytes(result.minified_bytes)} min${suffix}`;
};
