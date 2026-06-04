import type { CompressionFormat } from "../ui/format.js";
import { formatBytes } from "../ui/format.js";
import type { FileSizeResponse } from "../ipc/protocol.js";

const compressionLabels: Record<Exclude<CompressionFormat, "all">, string> = {
  brotli: "br",
  gzip: "gz",
  zstd: "zstd",
};

const compressedBytes = (response: FileSizeResponse, compression: CompressionFormat): number => {
  if (compression === "gzip") {
    return response.gzip_bytes;
  }

  if (compression === "zstd") {
    return response.zstd_bytes;
  }

  return response.brotli_bytes;
};

const compressionLabel = (compression: CompressionFormat): string =>
  compression === "all" ? "br" : compressionLabels[compression];

export const formatCurrentFileSizeSummary = (
  response: FileSizeResponse,
  compression: CompressionFormat,
): string => {
  const importCount = response.imports.length;
  const importLabel = importCount === 1 ? "import" : "imports";

  return [
    `Current file: ${formatBytes(compressedBytes(response, compression))} ${compressionLabel(compression)}`,
    `${formatBytes(response.minified_bytes)} min`,
    `${importCount} ${importLabel}`,
  ].join(" · ");
};
