import type { ImportLensConfig } from "../config.js";

export const shouldShowInlayHints = (config: ImportLensConfig): boolean =>
  config.enabled && config.display === "inlayHint";

export const shouldShowNativeInlayHints = (config: ImportLensConfig): boolean =>
  shouldShowInlayHints(config) && config.inlineRenderer === "native";

export const shouldShowColoredSourceHovers = (config: ImportLensConfig): boolean =>
  shouldShowInlayHints(config) && config.inlineRenderer === "colored";

export const shouldShowDecorations = (config: ImportLensConfig): boolean =>
  config.enabled
  && (
    shouldShowColoredSourceHovers(config)
    || (config.display !== "inlayHint" && !config.useCodeLens)
  );

export const shouldShowCodeLens = (config: ImportLensConfig): boolean =>
  config.enabled && config.display !== "inlayHint" && config.useCodeLens;

export const shouldOfferImportCompletions = (config: ImportLensConfig): boolean =>
  config.enabled;
