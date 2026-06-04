import type { ImportResult } from "../ipc/protocol.js";

export const isTypesOnlyResult = (result: ImportResult): boolean =>
  result.diagnostics.some((diagnostic) => diagnostic.stage === "types_only");
