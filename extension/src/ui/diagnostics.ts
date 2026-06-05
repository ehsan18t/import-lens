import type { ImportResult } from "../ipc/protocol.js";

export const copyImportDiagnosticsCommand = "importLens.copyImportDiagnostics";

export const formatImportDiagnostics = (result: ImportResult): string => {
  const lines = [`ImportLens diagnostics for ${result.specifier}`, ""];
  lines.push(`Confidence: ${result.confidence}`, "");

  if (result.confidence_reasons.length > 0) {
    lines.push("Confidence reasons:");
    for (const reason of result.confidence_reasons) {
      lines.push(`- ${reason}`);
    }
    lines.push("");
  }

  if (result.error) {
    lines.push(`Error: ${result.error}`, "");
  }

  if (result.diagnostics.length === 0) {
    lines.push("No daemon diagnostics were provided.");
    return lines.join("\n");
  }

  for (const diagnostic of result.diagnostics) {
    lines.push(`[${diagnostic.stage}] ${diagnostic.message}`);

    for (const detail of diagnostic.details) {
      lines.push(detail);
    }

    lines.push("");
  }

  return lines.join("\n").trimEnd();
};
