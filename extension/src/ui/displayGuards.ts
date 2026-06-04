import type { ImportLensConfig } from "../config.js";

export const shouldShowInlayHints = (config: ImportLensConfig): boolean =>
  config.enabled && config.display === "inlayHint";

export const shouldShowDecorations = (config: ImportLensConfig): boolean =>
  config.enabled && config.display !== "inlayHint" && !config.useCodeLens;

export const shouldShowCodeLens = (config: ImportLensConfig): boolean =>
  config.enabled && config.display !== "inlayHint" && config.useCodeLens;

export const shouldOfferImportCompletions = (config: ImportLensConfig): boolean =>
  config.enabled;
