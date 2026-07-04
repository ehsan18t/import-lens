import * as vscode from "vscode";
import type { ImportLensConfig } from "../config.js";
import { languageSelector } from "../languages.js";
import { nextCodeLensRegistrationAction } from "./codeLensRegistrationPolicy.js";
import type { ImportLensCodeLensProvider } from "./codelens.js";

export const syncCodeLensRegistration = (
  config: ImportLensConfig,
  provider: ImportLensCodeLensProvider,
  context: vscode.ExtensionContext,
  currentRegistration: vscode.Disposable | undefined,
): vscode.Disposable | undefined => {
  switch (nextCodeLensRegistrationAction(config, Boolean(currentRegistration))) {
    case "dispose":
      currentRegistration?.dispose();
      return undefined;
    case "register": {
      const registration = vscode.languages.registerCodeLensProvider(languageSelector, provider);
      context.subscriptions.push(registration);
      return registration;
    }
    default:
      return currentRegistration;
  }
};
