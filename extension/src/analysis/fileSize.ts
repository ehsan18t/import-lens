import {
  bytesForCompression,
  formatBytes,
  labelForCompression,
  type CompressionFormat,
} from "../ui/format.js";
import type { FileSizeResponse } from "../ipc/protocol.js";

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
