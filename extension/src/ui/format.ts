import type { ImportResult } from "../ipc/protocol.js";

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

const formatWarningSuffix = (result: ImportResult, showWarnings: boolean): string => {
  if (result.is_cjs) {
    return " CJS";
  }

  if (showWarnings && (result.side_effects || !result.truly_treeshakeable)) {
    return " ⚠";
  }

  return "";
};

export const formatImportSize = (result: ImportResult, options: FormatOptions): string => {
  if (result.error) {
    return "Size unavailable";
  }

  if (options.display === "verbose" || options.compression === "all") {
    return `${formatBytes(result.minified_bytes)} min · ${formatBytes(result.gzip_bytes)} gz · ${formatBytes(result.brotli_bytes)} br · ${formatBytes(result.zstd_bytes)} zstd${formatWarningSuffix(result, options.showWarnings)}`;
  }

  const compressedBytes = bytesForCompression(result, options.compression);
  const compressed = formatBytes(compressedBytes);
  const suffix = formatWarningSuffix(result, options.showWarnings);

  if (options.display === "minimal" || options.display === "inlayHint") {
    return `${compressed}${suffix}`;
  }

  return `${formatBytes(result.minified_bytes)} → ${compressed} (${labelForCompression(options.compression)})${suffix}`;
};

