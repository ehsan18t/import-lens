import type { FileSizeResponse } from "../ipc/protocol.js";
import {
  bytesForCompression,
  type CompressionFormat,
  formatBytes,
  labelForCompression,
} from "../ui/format.js";

export const formatCurrentFileSizeSummary = (
  response: FileSizeResponse,
  compression: CompressionFormat,
): string => {
  const importCount = response.imports.length;
  const importLabel = importCount === 1 ? "import" : "imports";

  return [
    `Current file: ${formatBytes(bytesForCompression(response, compression))} ${labelForCompression(compression)}`,
    `${formatBytes(response.minified_bytes)} min`,
    `${importCount} ${importLabel}`,
  ].join(" · ");
};
