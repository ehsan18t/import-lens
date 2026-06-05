import type { ImportLensConfig } from "../config.js";

export const shouldShowInlayHints = (config: ImportLensConfig): boolean =>
  config.enabled && config.display === "inlayHint";

export const shouldShowNativeInlayHints = (config: ImportLensConfig): boolean =>
  shouldShowInlayHints(config) && config.inlineRenderer === "native";

export const shouldShowDecorations = (config: ImportLensConfig): boolean =>
  config.enabled
  && (
    (config.display === "inlayHint" && config.inlineRenderer === "colored")
    || (config.display !== "inlayHint" && !config.useCodeLens)
  );

export const shouldShowCodeLens = (config: ImportLensConfig): boolean =>
  config.enabled && config.display !== "inlayHint" && config.useCodeLens;

export const shouldOfferImportCompletions = (config: ImportLensConfig): boolean =>
  config.enabled;
