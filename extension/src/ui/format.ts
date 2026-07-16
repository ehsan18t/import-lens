import type { ConfidenceLevel, ImportResult, ImportRuntime } from "../ipc/protocol.js";
import type { InlineHintTone } from "./inlineHintVisuals.js";
import {
  isNativeBinaryOnlyResult,
  isNativeBinaryResult,
  isTypesOnlyResult,
} from "./resultDiagnostics.js";

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

/**
 * The five sizes of a build that succeeded — the TypeScript half of ADR-0006's `MeasuredSizes`.
 *
 * Structurally a `CompressionByteSizes`, so it is what `bytesForCompression` takes: the only way
 * to get a number out of a result is to have gone through the `null` check first.
 */
export interface MeasuredSizes extends CompressionByteSizes {
  raw_bytes: number;
  minified_bytes: number;
}

/**
 * The sizes an import result carries, or `null` when it carries none.
 *
 * **This is the question.** Every consumer that shows, sums, compares, budgets or persists a size
 * asks it here, and the compiler will not let it be skipped. It replaces `!result.error`, the
 * negative check that produced the same defect six times: a transiently-degraded result carried
 * `error: null` and a fabricated size, and so passed every one of them.
 *
 * Loading and Unmeasured are both `null` here, and a consumer that needs to tell them apart looks
 * at the state's `status` (Loading has no result at all) or at `unmeasured_stage`.
 */
export const measuredSizes = (result: ImportResult | undefined): MeasuredSizes | null => {
  if (
    !result ||
    result.raw_bytes === null ||
    result.minified_bytes === null ||
    result.gzip_bytes === null ||
    result.brotli_bytes === null ||
    result.zstd_bytes === null
  ) {
    return null;
  }

  return {
    raw_bytes: result.raw_bytes,
    minified_bytes: result.minified_bytes,
    gzip_bytes: result.gzip_bytes,
    brotli_bytes: result.brotli_bytes,
    zstd_bytes: result.zstd_bytes,
  };
};

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

/** A size that may not exist — an unmeasured report row, say — rendered without inventing one. */
export const formatOptionalBytes = (bytes: number | null | undefined): string =>
  typeof bytes === "number" ? formatBytes(bytes) : "—";

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

  if (isNativeBinaryOnlyResult(result)) {
    tags.push("native binary only");
  } else if (isNativeBinaryResult(result)) {
    tags.push("native binary");
  }

  if (showWarnings && result.is_cjs) {
    tags.push("CJS");
  }

  return tags;
};

export const formatImportSizePrimary = (result: ImportResult, options: FormatOptions): string => {
  // "Is there a size?", not "is there an error?". The two used to coincide only by accident, and
  // that accident is what six rounds of fixes kept rediscovering.
  const sizes = measuredSizes(result);

  if (!sizes) {
    return "Size unavailable";
  }

  if (options.display === "verbose" || options.compression === "all") {
    return `${confidencePrefix(result)}${formatBytes(sizes.brotli_bytes)} br · ${formatBytes(sizes.gzip_bytes)} gz · ${formatBytes(sizes.zstd_bytes)} zstd · ${formatBytes(sizes.minified_bytes)} min`;
  }

  const compressedBytes = bytesForCompression(sizes, options.compression);
  const compressed = formatBytes(compressedBytes);
  const label = labelForCompression(options.compression);

  if (options.display === "minimal" || options.display === "inlayHint") {
    return `${confidencePrefix(result)}${compressed} ${label}`;
  }

  return `${confidencePrefix(result)}${compressed} ${label} · ${formatBytes(sizes.minified_bytes)} min`;
};
