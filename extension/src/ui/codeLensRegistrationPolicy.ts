import type { ImportLensConfig } from "../config.js";
import { shouldShowCodeLens } from "./displayGuards.js";

export type CodeLensRegistrationAction = "register" | "dispose" | "noop";

export const nextCodeLensRegistrationAction = (
  config: ImportLensConfig,
  hasRegistration: boolean,
): CodeLensRegistrationAction => {
  if (!shouldShowCodeLens(config)) {
    return hasRegistration ? "dispose" : "noop";
  }

  return hasRegistration ? "noop" : "register";
};
